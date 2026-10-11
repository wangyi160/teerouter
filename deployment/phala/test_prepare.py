import json
from pathlib import Path
import tempfile
import unittest
import prepare


class DeploymentTests(unittest.TestCase):
    def test_only_manifest_digest_is_accepted(self):
        for image in ("registry.example/router:latest", "registry.example/router@sha256:" + "0" * 64,
                      "registry.example/router@sha256:" + "a" * 64 + "\nprivileged: true"):
            with self.assertRaises(ValueError):
                prepare.render(image)
        result = prepare.render("registry.example/router@sha256:" + "a" * 64)
        self.assertIn("cap_drop: [ALL]", result)
        self.assertNotIn("__ROUTER_IMAGE_DIGEST__", result)

    def test_hash_uses_entire_object_and_rejects_ambiguous_json(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "app.json"
            obj = {"docker_compose_file": "services: {}", "public_logs": False}
            path.write_text(json.dumps(obj), encoding="utf-8")
            before = prepare.compose_hash(path)
            path.write_text(json.dumps(obj, sort_keys=True, indent=4), encoding="utf-8")
            self.assertEqual(before, prepare.compose_hash(path))
            obj["public_logs"] = True
            path.write_text(json.dumps(obj), encoding="utf-8")
            self.assertNotEqual(before, prepare.compose_hash(path))
            for content in ('{"docker_compose_file":"x","public_logs":false,"public_logs":true}',
                            '{"docker_compose_file":"x","number":NaN}'):
                path.write_text(content, encoding="utf-8")
                with self.assertRaises(ValueError):
                    prepare.compose_hash(path)


if __name__ == "__main__":
    unittest.main()
