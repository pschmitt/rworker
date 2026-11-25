"""Check the GitHub App token request: scope, permissions and failure handling."""
import importlib.util
import json
import sys
import tempfile
import unittest
from pathlib import Path
from unittest import mock

# The script path is the first argument (the Nix check passes the store copy).
SCRIPT = (
    Path(sys.argv.pop(1))
    if len(sys.argv) > 1
    else Path(__file__).resolve().parent.parent / "scripts" / "github-app-token.py"
)
spec = importlib.util.spec_from_file_location("github_app_token", SCRIPT)
module = importlib.util.module_from_spec(spec)
spec.loader.exec_module(module)


class Recorder:
    def __init__(self, token="ghs_test", actions="write"):
        self.calls = []
        self.token = token
        self.actions = actions

    def __call__(self, path, credential, payload=None):
        self.calls.append((path, credential, payload))
        if path.endswith("/installation"):
            return {"id": 42}
        return {"token": self.token, "permissions": {"actions": self.actions}}


class MintTest(unittest.TestCase):
    def setUp(self):
        self.directory = tempfile.TemporaryDirectory()
        self.addCleanup(self.directory.cleanup)
        self.app_id = Path(self.directory.name, "id")
        self.app_id.write_text("12345\n")

    def mint(self, recorder, repository="owner/project"):
        with mock.patch.object(module, "request", recorder), mock.patch.object(
            module, "app_jwt", return_value="jwt"
        ):
            return module.mint(str(self.app_id), "key", repository)

    def test_scope_is_one_repository_with_actions_write(self):
        recorder = Recorder()
        self.assertEqual(self.mint(recorder), "ghs_test")
        self.assertEqual(recorder.calls[0][0], "/repos/owner/project/installation")
        path, credential, payload = recorder.calls[1]
        self.assertEqual(path, "/app/installations/42/access_tokens")
        self.assertEqual(credential, "jwt")
        self.assertEqual(
            payload, {"repositories": ["project"], "permissions": {"actions": "write"}}
        )

    def test_rejects_broader_permissions_or_foreign_token(self):
        with self.assertRaises(ValueError):
            self.mint(Recorder(actions="read"))
        with self.assertRaises(ValueError):
            self.mint(Recorder(token="ghp_other"))

    def test_rejects_bad_inputs(self):
        with self.assertRaises(ValueError):
            self.mint(Recorder(), repository="project")
        self.app_id.write_text("abc")
        with self.assertRaises(ValueError):
            self.mint(Recorder())

    def test_failure_exits_without_leaking_details(self):
        with mock.patch.object(module, "mint", side_effect=RuntimeError("ghs_secret")):
            with mock.patch.object(sys, "stderr") as stderr:
                self.assertEqual(module.main(["a", "b", "c/d"]), 1)
        written = "".join(call.args[0] for call in stderr.write.call_args_list)
        self.assertNotIn("ghs_secret", written)

    def test_jwt_claims(self):
        with mock.patch.object(module.subprocess, "check_output", return_value=b"sig"):
            jwt = module.app_jwt("12345", "key", 1000)
        header, claims, signature = jwt.split(".")
        pad = lambda value: value + "=" * (-len(value) % 4)
        decode = lambda value: json.loads(module.base64.urlsafe_b64decode(pad(value)))
        self.assertEqual(decode(header)["alg"], "RS256")
        self.assertEqual(decode(claims), {"iat": 955, "exp": 1300, "iss": "12345"})


if __name__ == "__main__":
    unittest.main()
