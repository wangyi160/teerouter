use super::*;
use crate::upstream::Protocol;
use std::sync::{Arc, Mutex};

#[derive(Clone)]
pub(crate) struct Observer(Arc<Mutex<State>>, Arc<State>);

#[derive(Clone)]
struct State {
    provider: Provider,
    quote: Quote,
    dimensions: Dimensions,
    usage: Metrics,
    error: Option<&'static str>,
    final_usage: bool,
    auto_tier: bool,
    speed_unconfirmed: bool,
    geo_unconfirmed: bool,
    expected_tools: Vec<String>,
    hour_cache_requested: bool,
    hour_split_seen: bool,
    estimate: super::estimate::Estimate,
    upstream_status: Option<u16>,
    upstream_failed: bool,
    no_generation: bool,
    transport_phase: &'static str,
    request_flushed: bool,
}

fn tier(value: &str) -> Option<&str> {
    match value {
        "default" | "standard" | "standard_only" => Some("default"),
        "priority" | "fast" => Some("priority"),
        "flex" => Some("flex"),
        "scale" => Some("scale"),
        _ => None,
    }
}

impl Observer {
    pub(crate) fn new(provider: Provider, body: &Value, quote: Quote) -> Result<Self, ApiError> {
        for key in ["service_tier", "speed", "inference_geo"] {
            if body
                .get(key)
                .is_some_and(|v| !v.is_null() && !v.is_string())
            {
                return Err(unsupported("Invalid billing request dimension"));
            }
        }
        let requested = body
            .get("service_tier")
            .and_then(Value::as_str)
            .unwrap_or("default");
        let auto_tier = requested == "auto";
        let dimensions = Dimensions {
            service_tier: if auto_tier {
                "default"
            } else {
                tier(requested).ok_or_else(|| unsupported("Unsupported billing service tier"))?
            }
            .into(),
            speed: body
                .get("speed")
                .and_then(Value::as_str)
                .unwrap_or("standard")
                .into(),
            inference_geo: body
                .get("inference_geo")
                .and_then(Value::as_str)
                .unwrap_or("global")
                .into(),
        };
        if !matches!(dimensions.speed.as_str(), "standard" | "fast")
            || !matches!(dimensions.inference_geo.as_str(), "global" | "us")
        {
            return Err(unsupported(
                "Unsupported billing speed or inference geography",
            ));
        }
        if !auto_tier && rates(&quote, &dimensions, 0).is_err() {
            return Err(unsupported(
                "Official price for this service tier, speed and geography is missing",
            ));
        }
        if body.get("audio").is_some_and(|v| !v.is_null())
            || body.get("prediction").is_some_and(|v| !v.is_null())
            || body
                .get("modalities")
                .is_some_and(|v| v.as_array().is_none_or(|a| a.iter().any(|v| v != "text")))
            || body.get("search_parameters").is_some_and(|v| !v.is_null())
        {
            return Err(unsupported(
                "This request requires an unsupported billing rule",
            ));
        }
        let mut expected_tools = vec![];
        fn tool(
            provider: Provider,
            t: &Value,
            rules: &Rules,
            expected: &mut Vec<String>,
            depth: usize,
        ) -> Result<(), ApiError> {
            if depth > 8 {
                return Err(unsupported("Tool nesting exceeds billing limit"));
            }
            let kind = t.get("type").and_then(Value::as_str).unwrap_or("custom");
            if matches!(kind, "function" | "custom") {
                return Ok(());
            }
            if kind == "namespace" {
                for t in t["tools"]
                    .as_array()
                    .ok_or_else(|| unsupported("Invalid tool namespace"))?
                {
                    tool(provider, t, rules, expected, depth + 1)?;
                }
                return Ok(());
            }
            if provider == Provider::Anthropic
                && ["bash_", "text_editor_", "computer_"]
                    .iter()
                    .any(|s| kind.starts_with(s))
            {
                return Ok(());
            }
            let name = if provider == Provider::Anthropic && kind.starts_with("web_search_") {
                "web_search"
            } else {
                kind
            };
            if !supported_tool(provider, name) || !rules.tools.contains_key(name) {
                return Err(unsupported(
                    "Official billing rule for this server-side tool is unavailable",
                ));
            }
            // xAI REST currently exposes an aggregate invocation count, not a
            // guaranteed per-tool breakdown. Different tools cannot be guessed.
            if provider == Provider::XAI {
                return Err(unsupported(
                    "Per-tool list-price usage is not available on this xAI route",
                ));
            }
            if !expected.iter().any(|s| s == name) {
                expected.push(name.into());
            }
            Ok(())
        }
        if let Some(tools) = body.get("tools").and_then(Value::as_array) {
            for t in tools {
                tool(provider, t, &quote.billing_rules, &mut expected_tools, 0)?;
            }
        }
        fn hour(v: &Value, depth: usize) -> bool {
            if depth > 64 {
                return true;
            }
            match v {
                Value::Object(o) => {
                    o.get("cache_control")
                        .is_some_and(|v| v.get("ttl").is_some_and(|v| v == "1h"))
                        || o.values().any(|v| hour(v, depth + 1))
                }
                Value::Array(a) => a.iter().any(|v| hour(v, depth + 1)),
                _ => false,
            }
        }
        let state = State {
            provider,
            quote,
            speed_unconfirmed: dimensions.speed != "standard",
            geo_unconfirmed: dimensions.inference_geo != "global",
            dimensions,
            usage: Metrics::default(),
            error: None,
            final_usage: false,
            auto_tier: auto_tier || tier(requested) != Some("default"),
            expected_tools,
            hour_cache_requested: hour(body, 0),
            hour_split_seen: false,
            estimate: super::estimate::Estimate::new(body),
            upstream_status: None,
            upstream_failed: false,
            no_generation: false,
            transport_phase: "not_started",
            request_flushed: false,
        };
        Ok(Self(Arc::new(Mutex::new(state.clone())), Arc::new(state)))
    }

    /// Retries never inherit usage, output text, dimensions or errors from another call.
    pub(crate) fn fresh(&self) -> Self {
        Self(Arc::new(Mutex::new((*self.1).clone())), self.1.clone())
    }

    pub(crate) fn headers(&self, status: u16) {
        if let Ok(mut s) = self.0.lock() {
            s.upstream_status = Some(status);
            s.transport_phase = "response_headers";
        }
    }

    pub(crate) fn request_connecting(&self) {
        if let Ok(mut s) = self.0.lock() {
            s.transport_phase = "connecting";
        }
    }
    pub(crate) fn request_body(&self, body: &Value) {
        if let Ok(mut s) = self.0.lock() {
            s.estimate = super::estimate::Estimate::new(body);
        }
    }
    pub(crate) fn request_writing(&self) {
        if let Ok(mut s) = self.0.lock()
            && s.upstream_status.is_none()
        {
            s.transport_phase = "writing_request";
        }
    }
    pub(crate) fn request_flushed(&self) {
        if let Ok(mut s) = self.0.lock() {
            s.request_flushed = true;
            if s.upstream_status.is_none() {
                s.transport_phase = "awaiting_response";
            }
        }
    }

    pub(crate) fn no_generation(&self) {
        if let Ok(mut s) = self.0.lock() {
            s.no_generation = true;
        }
    }

    pub(crate) fn observe(&self, protocol: Protocol, value: &Value, streaming: bool) {
        if let Ok(mut state) = self.0.lock() {
            if state.upstream_status.is_some() {
                state.transport_phase = "receiving_response";
            }
            state.estimate.observe(protocol, value, streaming);
            if let Err(error) = state.observe(protocol, value, streaming) {
                state.error = Some(error);
            }
        }
    }

    pub(crate) fn finish(&self, status: u16, attempts: usize) -> Value {
        let Ok(s) = self.0.lock() else {
            return json!({"version":1,"state":"pending","reason":"billing_observer_unavailable"});
        };
        let mut usage = s.usage.clone();
        let mut source = "official";
        let mut estimate = Value::Null;
        let metered = s.usage.input.is_some() || s.usage.output.is_some();
        let processing_evidence = [
            s.usage.input,
            s.usage.output,
            s.usage.cache_read,
            s.usage.cache_write,
        ]
        .iter()
        .any(|n| n.is_some_and(|n| n > 0))
            || s.estimate.generated();
        let failed =
            status >= 400 || s.upstream_status.is_some_and(|n| n >= 400) || s.upstream_failed;
        let rejected = s.no_generation || s.upstream_status.is_some_and(|n| n >= 300);
        // A failed exchange without evidence of completed transmission or model
        // processing is closed at zero. This is a billing policy, not a claim
        // that the remote application definitely did no work.
        let before_send = failed && !s.request_flushed && s.upstream_status.is_none();
        let waiting =
            s.request_flushed || s.upstream_status.is_some_and(|n| (200..300).contains(&n));
        let refused = s.upstream_status.is_some_and(|n| (300..500).contains(&n));
        let no_charge = refused
            || (s.error.is_none()
                && (rejected || before_send)
                && !s.estimate.generated()
                && [
                    s.usage.input,
                    s.usage.output,
                    s.usage.total,
                    s.usage.cache_read,
                    s.usage.cache_write,
                    s.usage.reasoning,
                ]
                .iter()
                .all(|n| n.unwrap_or(0) == 0)
                && s.usage.tools.values().all(|n| *n == 0));
        let mut missing = None;
        if no_charge {
            usage = Metrics {
                input: Some(0),
                output: Some(0),
                ..Metrics::default()
            };
            source = "none";
        } else if !s.final_usage && s.error.is_none() {
            // Missing usage is not itself a generation failure. In particular,
            // successful JSON/SSE responses must never fall back to estimates.
            if !failed || !s.estimate.model_request || !(processing_evidence || waiting) {
                missing = Some("final_usage_missing");
            } else {
                let local_output = s.estimate.output();
                let cache = usage
                    .cache_read
                    .unwrap_or(0)
                    .checked_add(usage.cache_write.unwrap_or(0));
                let local_input = s.estimate.input().zip(cache).map(|(n, cache)| {
                    if s.provider == Provider::Anthropic {
                        n.saturating_sub(cache)
                    } else {
                        n.max(cache)
                    }
                });
                source = if metered { "mixed" } else { "estimated" };
                let input_local = usage.input.is_none();
                let output_local = local_output.is_some_and(|n| n > usage.output.unwrap_or(0))
                    || usage.output.is_none();
                if input_local {
                    usage.input = local_input;
                }
                if output_local {
                    usage.output = local_output;
                }
                // Partial counters cannot establish the final total or hidden reasoning.
                // Preserve the original counters separately as evidence.
                if input_local || output_local {
                    usage.total = None;
                    usage.reasoning = None;
                }
                if local_output.is_none() || usage.input.is_none() || usage.output.is_none() {
                    missing = Some("estimation_limit_or_usage_missing");
                }
                estimate = json!({"method":"o200k_json_text_v1","input_tokens":local_input,
                    "visible_output_tokens":local_output,"input_source":if input_local {"local"} else {"upstream"},
                    "output_source":if output_local {"local"} else {"upstream"},
                    "cache_policy":if usage.cache_read.is_none() && usage.cache_write.is_none() {"ordinary_input"} else {"upstream"},
                    "context_incomplete":s.estimate.context_incomplete,"hidden_output_included":false});
            }
        }
        let invalid_reported = calculate(s.provider, &s.quote, &s.usage, &s.dimensions)
            .err()
            .filter(|reason| reason.starts_with("inconsistent_") || *reason == "usage_overflow");
        let error = if no_charge {
            None
        } else {
            s.error.or(invalid_reported).or(missing).or_else(|| {
                if attempts > 1 {
                    Some("multiple_upstream_attempts")
                } else if s.auto_tier {
                    Some("actual_service_tier_missing")
                } else if s.speed_unconfirmed {
                    Some("actual_speed_missing")
                } else if s.geo_unconfirmed {
                    Some("actual_inference_geo_missing")
                } else if s
                    .expected_tools
                    .iter()
                    .any(|t| !s.usage.tools.contains_key(t))
                {
                    Some("tool_usage_missing")
                } else if s.hour_cache_requested
                    && s.usage.cache_write.unwrap_or(0) > 0
                    && !s.hour_split_seen
                {
                    Some("cache_duration_usage_missing")
                } else {
                    None
                }
            })
        };
        let mut record = result(s.provider, &s.quote, &usage, &s.dimensions, error);
        record["usage_source"] = source.into();
        record["coverage"] = if source == "estimated" || source == "mixed" {
            "partial"
        } else if record["state"] == "calculated" {
            "complete"
        } else {
            "unknown"
        }
        .into();
        record["upstream_status"] = s.upstream_status.into();
        record["transport"] = json!({"version":1,"phase":s.transport_phase,
            "request_flushed":s.request_flushed});
        record["termination"] = if status == 499 {
            "request_cancelled"
        } else if s.no_generation {
            "not_forwarded"
        } else if no_charge && s.upstream_status.is_some() {
            "upstream_rejected"
        } else if status == 504 {
            "timeout"
        } else if failed {
            "upstream_error"
        } else {
            "completed"
        }
        .into();
        if s.upstream_failed {
            record["upstream_failed"] = true.into();
        }
        if source == "none" {
            if metered {
                record["waived_usage"] = json!(s.usage);
            }
            record["reason"] = if s.no_generation {
                "no_generation"
            } else if rejected {
                "upstream_rejected"
            } else {
                "request_not_fully_sent"
            }
            .into();
        }
        if !estimate.is_null() {
            record["estimate"] = estimate;
            record["reported_usage"] = json!(s.usage);
        }
        record
    }
}

fn number(v: &Value, key: &str, dest: &mut Option<u64>) -> Checked<()> {
    if let Some(value) = v.get(key).filter(|v| !v.is_null()) {
        *dest = Some(value.as_u64().ok_or("invalid_usage_number")?);
    }
    Ok(())
}

fn object(v: &Value, key: &str) -> Checked<()> {
    if v.get(key).is_some_and(|v| !v.is_null() && !v.is_object()) {
        return Err("invalid_usage_object");
    }
    Ok(())
}

impl State {
    fn observe(&mut self, protocol: Protocol, value: &Value, streaming: bool) -> Checked<()> {
        let envelope = value
            .get("response")
            .or_else(|| value.get("message"))
            .unwrap_or(value);
        // Some providers report model errors inside an HTTP 200 response/SSE.
        self.upstream_failed |= value.get("error").is_some_and(|v| !v.is_null())
            || envelope.get("error").is_some_and(|v| !v.is_null())
            || matches!(
                value["type"].as_str(),
                Some("error" | "response.failed" | "response.cancelled")
            )
            || matches!(envelope["status"].as_str(), Some("failed" | "cancelled"));
        // Hosted tools can carry fees that are absent from the token usage.
        let mut check_item = |item: &Value| -> Checked<()> {
            let kind = item.get("type").and_then(Value::as_str).unwrap_or("");
            if self.provider == Provider::OpenAI
                && matches!(
                    kind,
                    "web_search_call"
                        | "file_search_call"
                        | "code_interpreter_call"
                        | "image_generation_call"
                        | "mcp_call"
                )
            {
                return Err("unsupported_tool_usage");
            }
            if self.provider == Provider::Anthropic && kind == "server_tool_use" {
                let name = item.get("name").and_then(Value::as_str).unwrap_or("");
                if name != "web_search" || !self.quote.billing_rules.tools.contains_key(name) {
                    return Err("tool_price_missing");
                }
                if !self.expected_tools.iter().any(|t| t == name) {
                    self.expected_tools.push(name.into());
                }
            }
            Ok(())
        };
        for key in ["item", "content_block"] {
            if let Some(item) = value.get(key) {
                check_item(item)?;
            }
        }
        for key in ["output", "content"] {
            if let Some(items) = envelope.get(key).and_then(Value::as_array) {
                for item in items {
                    check_item(item)?;
                }
            }
        }
        object(value, "usage")?;
        object(envelope, "usage")?;
        let usage = value
            .get("usage")
            .filter(|v| v.is_object())
            .or_else(|| envelope.get("usage"));
        if let Some(t) = envelope
            .get("service_tier")
            .or_else(|| usage.and_then(|v| v.get("service_tier")))
            .filter(|v| !v.is_null())
        {
            let selected = t.as_str().ok_or("invalid_service_tier")?;
            if selected == "auto" {
                // Responses can echo "auto" in response.created/in_progress,
                // then report the actual tier with the final usage. It is a
                // selector, not a price or a permanent observation error.
                // If it never resolves, finish() keeps the bill pending.
                self.auto_tier = true;
            } else {
                self.dimensions.service_tier = tier(selected).ok_or("unknown_service_tier")?.into();
                self.auto_tier = false;
            }
        }
        if let Some(geo) = usage
            .and_then(|v| v.get("inference_geo"))
            .filter(|v| !v.is_null())
        {
            self.dimensions.inference_geo = match geo.as_str() {
                Some("global") => "global",
                Some("us") => "us",
                _ => return Err("unknown_inference_geo"),
            }
            .into();
            self.geo_unconfirmed = false;
        }
        if let Some(speed) = usage.and_then(|v| v.get("speed")).filter(|v| !v.is_null()) {
            self.dimensions.speed = match speed.as_str() {
                Some("standard") => "standard",
                Some("fast") => "fast",
                _ => return Err("unknown_speed"),
            }
            .into();
            self.speed_unconfirmed = false;
        }
        let Some(u) = usage.filter(|v| v.is_object()) else {
            return Ok(());
        };
        let chat = u.get("prompt_tokens").is_some()
            || (protocol == Protocol::Chat && u.get("input_tokens").is_none());
        let (i, o, di, do_) = if chat {
            (
                "prompt_tokens",
                "completion_tokens",
                "prompt_tokens_details",
                "completion_tokens_details",
            )
        } else {
            (
                "input_tokens",
                "output_tokens",
                "input_tokens_details",
                "output_tokens_details",
            )
        };
        number(u, i, &mut self.usage.input)?;
        number(u, o, &mut self.usage.output)?;
        number(u, "total_tokens", &mut self.usage.total)?;
        number(
            u,
            "cost_in_usd_ticks",
            &mut self.usage.reported_cost_in_usd_ticks,
        )?;
        if protocol == Protocol::Messages {
            object(u, "cache_creation")?;
            object(u, "server_tool_use")?;
            number(u, "cache_read_input_tokens", &mut self.usage.cache_read)?;
            number(
                u,
                "cache_creation_input_tokens",
                &mut self.usage.cache_write,
            )?;
            if let Some(cache) = u.get("cache_creation").filter(|v| !v.is_null()) {
                number(
                    cache,
                    "ephemeral_1h_input_tokens",
                    &mut self.usage.cache_write_hour,
                )?;
                self.hour_split_seen |= cache
                    .get("ephemeral_1h_input_tokens")
                    .and_then(Value::as_u64)
                    .is_some();
                let mut short = None;
                number(cache, "ephemeral_5m_input_tokens", &mut short)?;
                if let (Some(short), Some(hour), Some(total)) =
                    (short, self.usage.cache_write_hour, self.usage.cache_write)
                    && short.checked_add(hour) != Some(total)
                {
                    return Err("inconsistent_cache_usage");
                }
            }
            if let Some(tools) = u.get("server_tool_use").and_then(Value::as_object) {
                for (key, v) in tools {
                    let n = v.as_u64().ok_or("invalid_tool_usage")?;
                    match key.as_str() {
                        "web_search_requests" => {
                            self.usage.tools.insert("web_search".into(), n);
                        }
                        _ if n > 0 => return Err("unsupported_tool_usage"),
                        _ => (),
                    }
                }
            }
            // Iterations may cross context-price boundaries independently.
            // A single message entry is just a breakdown of the top-level total.
            if u.get("iterations")
                .is_some_and(|v| !v.is_null() && !v.is_array())
            {
                return Err("invalid_usage_object");
            }
            if let Some(iterations) = u.get("iterations").and_then(Value::as_array)
                && (iterations.len() > 1 || iterations.iter().any(|i| i["type"] != "message"))
            {
                return Err("iteration_pricing_unavailable");
            }
        } else {
            object(u, di)?;
            object(u, do_)?;
            number(&u[di], "cached_tokens", &mut self.usage.cache_read)?;
            let (mut creation, mut write) = (None, None);
            number(&u[di], "cache_creation_tokens", &mut creation)?;
            number(&u[di], "cache_write_tokens", &mut write)?;
            if creation.is_some() && write.is_some() && creation != write {
                return Err("inconsistent_cache_usage");
            }
            if let Some(write) = creation.or(write) {
                self.usage.cache_write = Some(write);
            }
            number(
                &u[di],
                "cache_creation_1h_tokens",
                &mut self.usage.cache_write_hour,
            )?;
            self.hour_split_seen |= u[di]
                .get("cache_creation_1h_tokens")
                .and_then(Value::as_u64)
                .is_some();
            number(&u[do_], "reasoning_tokens", &mut self.usage.reasoning)?;
            for details in [&u[di], &u[do_]] {
                for field in [
                    "audio_tokens",
                    "image_tokens",
                    "video_tokens",
                    "rejected_prediction_tokens",
                ] {
                    if let Some(v) = details.get(field).filter(|v| !v.is_null())
                        && v.as_u64().ok_or("invalid_usage_number")? > 0
                    {
                        return Err("unsupported_modality_usage");
                    }
                }
            }
            for key in ["num_sources_used", "num_server_side_tools_used"] {
                if let Some(v) = u.get(key).filter(|v| !v.is_null())
                    && v.as_u64().ok_or("invalid_tool_usage")? > 0
                {
                    return Err("tool_usage_breakdown_missing");
                }
            }
        }
        let final_event = !streaming
            || match protocol {
                Protocol::Chat => u.get(i).is_some() && u.get(o).is_some(),
                Protocol::Messages => {
                    value["type"] == "message_delta"
                        && u.get(o).is_some()
                        && value["delta"]
                            .get("stop_reason")
                            .is_some_and(|v| !v.is_null())
                }
                Protocol::Responses => matches!(
                    value["type"].as_str(),
                    Some(
                        "response.completed"
                            | "response.incomplete"
                            | "response.failed"
                            | "response.cancelled"
                    )
                ),
            };
        self.final_usage |=
            final_event && self.usage.input.is_some() && self.usage.output.is_some();
        Ok(())
    }
}
