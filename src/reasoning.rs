//! Change only the field location required by a protocol bridge, never the value.
use crate::Error;
use serde_json::{Map, Value};

pub(crate) fn effort(
    source: Option<&Value>,
    target: &mut Value,
    path: &[&str],
) -> Result<(), Error> {
    let Some(value) = source else {
        return Ok(());
    };
    let (key, parents) = path.split_last().expect("nonempty effort path");
    let mut node = target;
    for parent in parents {
        let map = node.as_object_mut().ok_or(Error::InvalidRequest(
            "reasoning container must be an object",
        ))?;
        node = map
            .entry((*parent).to_owned())
            .or_insert_with(|| Value::Object(Map::new()));
    }
    let map = node.as_object_mut().ok_or(Error::InvalidRequest(
        "reasoning container must be an object",
    ))?;
    if map.get(*key).is_some_and(|existing| existing != value) {
        return Err(Error::InvalidRequest("conflicting reasoning effort fields"));
    }
    map.insert((*key).to_owned(), value.clone());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    #[test]
    fn effort_is_lossless_and_conflicts_never_silently_win() {
        for value in [json!("future-EXTREME-v9"), json!(4097), Value::Null] {
            let mut target = json!({"reasoning":{"summary":"auto"}});
            effort(Some(&value), &mut target, &["reasoning", "effort"]).unwrap();
            assert_eq!(target["reasoning"]["effort"], value);
            assert_eq!(target["reasoning"]["summary"], "auto");
            assert!(
                effort(
                    Some(&json!("different")),
                    &mut target,
                    &["reasoning", "effort"]
                )
                .is_err()
            );
        }
        let mut absent = json!({});
        effort(None, &mut absent, &["reasoning", "effort"]).unwrap();
        assert_eq!(absent, json!({}));
    }
}
