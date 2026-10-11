"""Render a digest-pinned deployment or hash a reviewed app-compose. No cloud writes."""
import argparse
import hashlib
import json
from pathlib import Path
import re


def render(image: str) -> str:
    if not re.fullmatch(r"[a-z0-9][a-z0-9./_:-]*@sha256:[0-9a-f]{64}", image):
        raise ValueError("Supply the registry manifest digest, repository@sha256:...; tags are rejected")
    if image.endswith("0" * 64):
        raise ValueError("A placeholder digest is not a release")
    return Path(__file__).with_name("compose.yaml.in").read_text(encoding="utf-8").replace(
        "__ROUTER_IMAGE_DIGEST__", image)


def compose_hash(path: Path) -> str:
    # Hash the complete reviewed deployment JSON exported by Phala, not only YAML.
    # Reject duplicate keys and unsupported numbers rather than guessing a canonical form.
    def unique(pairs):
        obj = {}
        for k, v in pairs:
            if k in obj:
                raise ValueError("Duplicate JSON key: " + k)
            obj[k] = v
        return obj

    def reject_number(value):
        raise ValueError("Unsupported non-integer JSON number: " + value)

    obj = json.loads(path.read_text(encoding="utf-8"), object_pairs_hook=unique,
                     parse_float=reject_number, parse_constant=reject_number)
    if not isinstance(obj, dict) or not isinstance(obj.get("docker_compose_file"), str):
        raise ValueError("Expected the full dstack app-compose object")
    canonical = json.dumps(obj, sort_keys=True, separators=(",", ":"), ensure_ascii=False)
    return hashlib.sha256(canonical.encode("utf-8")).hexdigest()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    sub = parser.add_subparsers(dest="command", required=True)
    cmd = sub.add_parser("compose")
    cmd.add_argument("image")
    cmd.add_argument("--output", type=Path, required=True)
    cmd = sub.add_parser("hash")
    cmd.add_argument("app_compose", type=Path)
    args = parser.parse_args()
    if args.command == "compose":
        content = render(args.image)
        args.output.parent.mkdir(parents=True, exist_ok=True)
        args.output.write_text(content, encoding="utf-8", newline="\n")
        print(args.output)
    else:
        print(compose_hash(args.app_compose))


if __name__ == "__main__":
    main()
