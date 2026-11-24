"""Check MCP argument forwarding and registered job handling."""

import importlib.util
from pathlib import Path
import sys
from types import ModuleType
import unittest
from unittest.mock import patch


class FakeToolError(Exception):
    pass


class FakeServer:
    def __init__(self, _name):
        pass

    def tool(self):
        return lambda func: func


class WorkerMcpTest(unittest.TestCase):
    def setUp(self):
        modules = {name: ModuleType(name) for name in (
            "mcp", "mcp.server", "mcp.server.mcpserver", "mcp.server.mcpserver.exceptions",
        )}
        modules["mcp.server.mcpserver"].MCPServer = FakeServer
        modules["mcp.server.mcpserver.exceptions"].ToolError = FakeToolError
        spec = importlib.util.spec_from_file_location("worker_mcp", Path(__file__).resolve().parents[2] / "mcp" / "rworker-mcp.py")
        module = importlib.util.module_from_spec(spec)
        with patch.dict(sys.modules, modules), patch.object(sys, "argv", ["rworker-mcp", "--rworker", "rworker"]):
            spec.loader.exec_module(module)
        self.module = module

    def test_up_passes_profile_and_resource_overrides(self):
        module = self.module
        with patch.object(module, "run", return_value='{"name":"test"}') as run:
            self.assertEqual(module.up(profile="large-builder", disk_gib=500)["name"], "test")
        args = run.call_args.args
        self.assertIn("--profile", args)
        self.assertEqual(args[args.index("--profile") + 1], "large-builder")
        self.assertEqual(args[args.index("--disk") + 1], "500")
        with patch.object(module, "run", return_value='[{"name":"standard"}]') as run:
            self.assertEqual(module.profiles(), [{"name": "standard"}])
            run.assert_called_once_with("profiles", "--json", timeout=120)

    def test_credentials_opt_out_covers_up_and_exec(self):
        module = self.module
        with patch.object(module, "run", return_value="{}") as run:
            module.up(profile="gh-aarch64")
            self.assertNotIn("--no-credentials", run.call_args.args)
            module.up(profile="gh-aarch64", credentials=False)
            self.assertIn("--no-credentials", run.call_args.args)
            module.up(profile="gl-x86_64", nix_cache=False, nix_auth=False, niks3_token=False)
            for flag in ("--no-nix-cache", "--no-nix-auth", "--no-niks3-token"):
                self.assertIn(flag, run.call_args.args)
        with patch.object(module, "run_bounded", return_value=(0, '{"name":"w","exit_code":0,"stdout":"","stderr":""}', "", False)) as bounded:
            module.exec_ephemeral("true", credentials=False)
            self.assertIn("--no-credentials", bounded.call_args.args)

    def test_niks3_token_is_default_and_optional(self):
        module = self.module
        with patch.object(module, "run", return_value="{}") as run:
            module.up()
            self.assertNotIn("--no-niks3-token", run.call_args.args)
            module.up(niks3_token=False)
            self.assertIn("--no-niks3-token", run.call_args.args)
            module.extend("w", "2h")
            self.assertNotIn("--no-niks3-token", run.call_args.args)
            module.extend("w", "2h", niks3_token=False)
            self.assertIn("--no-niks3-token", run.call_args.args)
            module.setup_niks3_token("w")
            run.assert_called_with("setup", "niks3-token", "w", "--json", timeout=180)
            module.setup_niks3_token("w", force=True)
            run.assert_called_with("setup", "niks3-token", "w", "--json", "--force", timeout=180)
            module.topic("w", "rbw release build")
            self.assertEqual(
                run.call_args.args[:4], ("topic", "w", "rbw release build", "--json"))
            module.claim("w", "rbw release build", "claude-code:abc")
            self.assertEqual(
                run.call_args.args,
                ("claim", "w", "--topic", "rbw release build",
                 "--agent-id", "claude-code:abc", "--json"))
            module.up(agent_id="claude-code:abc")
            self.assertIn("--agent-id", run.call_args.args)
            module.ls(agent_id="claude-code:abc")
            self.assertEqual(
                run.call_args.args,
                ("ls", "--json", "--agent-id", "claude-code:abc"))

    def test_exec_uses_registered_job_and_returns_exit_status(self):
        module = self.module
        with patch.object(module, "run", side_effect=["job-123\n", "job-123\texit 7\ttoday\n", "output\n"]) as run:
            result = module.exec("worker", "exit 7")
        self.assertEqual(result["job"], "job-123")
        self.assertEqual(result["exit_code"], 7)
        self.assertIn("output", result["stdout"])
        self.assertEqual(run.call_args_list[0].args, ("start", "worker", "--", "exit 7"))

    def test_log_snapshot_flags(self):
        with patch.object(self.module, "run", return_value="output") as run:
            self.assertEqual(
                self.module.logs("worker", "job", cached=True, redact=True),
                "output",
            )
            run.assert_called_once_with(
                "logs", "--lines", "200", "worker", "job", "--cached",
                "--redact", timeout=60,
            )
            with self.assertRaises(FakeToolError):
                self.module.logs(cached=True)

    def test_setup_register_log_file(self):
        with patch.object(self.module, "run", return_value="") as run:
            self.assertEqual(self.module.setup_register_log_file("worker", "job", "/tmp/build.log"), "job")
            run.assert_called_once_with(
                "setup", "register-log-file", "worker", "job", "--log", "/tmp/build.log", timeout=60,
            )

    def test_setup_mesh_routes_by_provider(self):
        for provider in ("netbird", "tailscale"):
            with patch.object(self.module, "run", return_value='{"proxy":"socks5h://127.0.0.1:1080"}') as run:
                setup = getattr(self.module, f"setup_{provider}")
                self.assertIn("proxy", setup("worker"))
                run.assert_called_once_with("setup", provider, "worker", "--json", timeout=330)

    def test_retained_events_filter_and_limit(self):
        with patch.object(self.module, "run", return_value='[{"id":1},{"id":2}]') as run:
            self.assertEqual(self.module.events(agent_id="codex:abc", limit=1), [{"id": 2}])
            run.assert_called_once_with(
                "events", "--json", "--agent-id", "codex:abc", timeout=60,
            )


    def test_blob_upload_uses_grouped_setup_and_local_paths(self):
        with patch.object(self.module, "run", return_value='{"paths":["/nix/store/test"]}') as run:
            result = self.module.setup_nix_upload_blob("worker", ["-local file.zip"], "vendor.zip")
            self.assertEqual(result["paths"], ["/nix/store/test"])
            run.assert_called_once_with(
                "setup", "nix", "upload-blob", "worker", "--json", "--name", "vendor.zip",
                "--", "-local file.zip", timeout=1800,
            )
            with self.assertRaises(FakeToolError):
                self.module.setup_nix_upload_blob("worker", ["one", "two"], "vendor.zip")
            run.assert_called_once()

    def test_grouped_nix_setup_modes(self):
        with patch.object(self.module, "run", return_value="{}") as run:
            self.module.setup_nix("cache")
            run.assert_called_with("setup", "nix", "--json", "--", "cache", timeout=300)
            self.module.setup_nix("worker", auth=False)
            run.assert_called_with("setup", "nix", "cache", "--json", "worker", timeout=300)
            self.module.setup_nix("worker", cache=False, auth_hosts=["github.com"], dry_run=True)
            run.assert_called_with(
                "setup", "nix", "auth", "--json", "--auth-host", "github.com",
                "--dry-run", "worker", timeout=300,
            )

    def test_builder_keeps_hosted_ssh_config_and_legacy_gateway_result(self):
        with patch.object(self.module, "run", return_value='{"builder":"line"}') as run:
            self.assertEqual(self.module.setup_nix_builder(gateway="example.test", jobs=4), "line")
            run.assert_called_with(
                "setup", "nix", "builder", "--gateway", "example.test", "--jobs", "4",
                "--json", timeout=60,
            )
            run.return_value = '{"builder":"line","ssh_config":"relay config"}'
            self.assertEqual(self.module.setup_nix_builder("gh-worker")["ssh_config"], "relay config")

    def test_ephemeral_exec_returns_nonzero_command_result(self):
        output = '{"name":"w","exit_code":7,"stdout":"output","stderr":"","kept":true}'
        with patch.object(self.module, "run_bounded", return_value=(7, output, "", False)) as run:
            result = self.module.exec_ephemeral(
                "exit 7", arch="aarch64", queue_timeout=15, sync_sources=["src:app"],
                fetch_paths=["app/out:out"], keep_on_failure=True,
            )
        self.assertEqual(result["exit_code"], 7)
        self.assertTrue(result["kept"])
        run.assert_called_once_with(
            "exec", "--json", "--ttl", "2h", "--arch", "aarch64", "--queue-timeout", "15",
            "--sync", "src:app", "--fetch", "app/out:out", "--keep-on-failure", "--",
            "exit 7", seconds=1800,
        )

    def test_watch_returns_complete_json_on_timeout(self):
        with patch.object(
            self.module, "run_bounded",
            return_value=(-15, '{"id":1}\n{"id":2}\n{"id":', "", True),
        ) as run:
            self.assertEqual(
                self.module.events(watch_seconds=5, since_now=True, limit=1), [{"id": 2}],
            )
            run.assert_called_once_with(
                "events", "--json", "--watch", "--since-now", seconds=5,
            )

    def test_copy_and_image_upload_preserve_paths(self):
        with patch.object(self.module, "run", return_value="{}") as run:
            self.module.cp("file with spaces", "w:dir")
            run.assert_called_with("cp", "--json", "file with spaces", "w:dir", timeout=1800)
            self.module.image_upload("-disk.qcow2", "image", tag="release")
            run.assert_called_with(
                "image", "upload", "--json", "--name", "image", "--tag", "release",
                "--", "-disk.qcow2", timeout=3600,
            )


if __name__ == "__main__":
    unittest.main()
