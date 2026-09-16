import contextlib
from datetime import datetime, timezone
import io
import unittest
from unittest.mock import patch

import auto_release as release


class AutoReleaseTests(unittest.TestCase):
    def setUp(self):
        self.published = [{"draft": False, "prerelease": False,
                           "tag_name": "fastrace-diesel-0.4.0", "published_at": "2026-08-26T01:18:23Z"}]
        self.pr = {
            "number": 2, "state": "open", "draft": False, "user": {"login": "github-actions[bot]"},
            "head": {"ref": "release-plz-next", "sha": "checked-sha",
                     "repo": {"full_name": "codyps/fastrace-diesel"}},
            "base": {"ref": "main", "sha": "base-sha"}, "mergeable_state": "clean", "mergeable": True, "created_at": "2026-01-01T00:00:00Z",
        }
        self.runs = [{"id": 10, "status": "completed", "conclusion": "success"}]
        self.jobs = [{"name": name, "status": "completed", "conclusion": "success"}
                     for name in release.REQUIRED_JOBS]

    def test_release_pr_must_be_same_repository_and_default_branch(self):
        self.assertTrue(release.release_pr(self.pr, "codyps/fastrace-diesel", "main"))
        self.pr["head"]["repo"]["full_name"] = "someone/fastrace-diesel"
        self.assertFalse(release.release_pr(self.pr, "codyps/fastrace-diesel", "main"))
        self.pr["head"]["repo"] = None
        self.assertFalse(release.release_pr(self.pr, "codyps/fastrace-diesel", "main"))

    def test_missing_skipped_or_failed_checks_block(self):
        self.assertTrue(release.ci_passed(self.runs, self.jobs))
        self.assertFalse(release.ci_passed(self.runs, self.jobs[:-1]))
        for conclusion in ["failure", "skipped", None]:
            with self.subTest(conclusion=conclusion):
                self.jobs[0]["conclusion"] = conclusion
                self.assertFalse(release.ci_passed(self.runs, self.jobs))

    def test_newer_pending_run_blocks_older_success(self):
        self.runs.append({"id": 11, "status": "in_progress", "conclusion": None})
        self.assertFalse(release.ci_passed(self.runs, self.jobs))

    def run_main(self, merge=True, changed=False, unsafe_file=False, held=False):
        root = "repos/codyps/fastrace-diesel"
        reads = {
            root: {"default_branch": "main"}, "user": {"login": "github-actions[bot]"},
            f"{root}/pulls/2": self.pr,
        }
        collections = {
            f"{root}/pulls?state=open&base=main": [self.pr],
            f"{root}/pulls/2/files": [{"filename": "src/lib.rs" if unsafe_file else "Cargo.toml",
                                      "status": "modified"}],
            f"{root}/actions/workflows/ci.yml/runs?event=workflow_dispatch&head_sha=checked-sha": self.runs,
            f"{root}/actions/runs/10/jobs": self.jobs,
        }
        mutations = []
        pr_reads = 0

        def fake_api(endpoint, payload=None):
            nonlocal pr_reads
            if payload is not None:
                mutations.append((endpoint, payload))
                return {"merged": True, "sha": "merge-sha"}
            if endpoint.endswith("/pulls/2"):
                pr_reads += 1
                if changed and pr_reads == 2:
                    return {**self.pr, "head": {**self.pr["head"], "sha": "new-sha"}}
            return reads[endpoint]

        with (patch.object(release, "api", side_effect=fake_api),
              patch.object(release, "pages", side_effect=lambda endpoint, key=None:
                           collections.get(endpoint, [])),
              patch.object(release, "cooldown_reason", return_value="Hold-off" if held else None),
              patch.dict("os.environ", {"GITHUB_REPOSITORY": "codyps/fastrace-diesel"}),
              patch("sys.argv", ["auto_release.py"] + (["--merge"] if merge else [])),
              contextlib.redirect_stdout(io.StringIO())):
            release.main()
        return mutations

    def test_merge_pins_examined_sha_and_preserves_release_pr_history(self):
        self.assertEqual(self.run_main(), [
            ("repos/codyps/fastrace-diesel/pulls/2/merge", {"sha": "checked-sha", "merge_method": "merge"})])

    def test_preview_never_merges(self):
        self.assertEqual(self.run_main(merge=False), [])

    def test_changed_head_unsafe_files_or_new_release_block_merge(self):
        for option in ["changed", "unsafe_file", "held"]:
            with self.subTest(option=option):
                self.assertEqual(self.run_main(**{option: True}), [])

    def test_draft_wrong_author_and_blocked_mergeability(self):
        for changes in [{"draft": True}, {"user": {"login": "someone"}},
                        {"mergeable_state": "blocked"}, {"mergeable_state": "unknown"}]:
            with self.subTest(changes=changes), patch.dict(self.pr, changes):
                self.assertEqual(self.run_main(), [])

    def test_pagination_reads_all_pages(self):
        with patch.object(release, "api", side_effect=[[{}] * 100, [{"id": 101}]]) as api:
            self.assertEqual(len(release.pages("repos/example/releases")), 101)
            self.assertIn("page=2", api.call_args.args[0])

    def test_pagination_limit_fails_instead_of_using_partial_history(self):
        with patch.object(release, "api", return_value=[{}] * 100):
            with self.assertRaisesRegex(RuntimeError, "Pagination limit"):
                release.pages("repos/example/releases")


if __name__ == "__main__":
    unittest.main()
