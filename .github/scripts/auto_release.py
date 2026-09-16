"""Periodically merge a checked release-plz PR; default to a read-only preview."""

import argparse
from datetime import datetime, timedelta, timezone
import json
import os
import subprocess

INTERVAL = timedelta(days=14)
REQUIRED_JOBS = {"Quality", "Test"}
RELEASE_FILES = {"Cargo.toml", "Cargo.lock", "CHANGELOG.md"}


def timestamp(value):
    return datetime.fromisoformat(value.replace("Z", "+00:00"))


def api(endpoint, payload=None):
    command = ["gh", "api", endpoint]
    if payload is not None:
        command += ["--method", "PUT", "--input", "-"]
    result = subprocess.run(
        command, input=json.dumps(payload) if payload is not None else None,
        text=True, capture_output=True, check=True, timeout=30,
    )
    return json.loads(result.stdout)


def pages(endpoint, key=None):
    # Bound API work as well as each individual request. Never use partial data.
    rows = []
    separator = "&" if "?" in endpoint else "?"
    for page in range(1, 21):
        data = api(f"{endpoint}{separator}per_page=100&page={page}")
        batch = data[key] if key else data
        rows.extend(batch)
        if len(batch) < 100:
            return rows
    raise RuntimeError("Pagination limit reached; manual inspection required")


def release_pr(pr, repository, branch):
    return (
        pr["head"]["ref"].startswith("release-plz-")
        and (pr["head"].get("repo") or {}).get("full_name") == repository
        and pr["base"]["ref"] == branch
    )


def cooldown_reason(releases, closed_prs, now, pr_date):
    published = [timestamp(r["published_at"]) for r in releases
                 if not r["draft"] and not r["prerelease"]
                 and r["tag_name"].startswith("diesel-fastrace-") and r["published_at"]]
    latest = max(published) if published else datetime.min.replace(tzinfo=timezone.utc)
    if any(p["merged_at"] and timestamp(p["merged_at"]) > latest for p in closed_prs):
        return "A merged release PR is still awaiting publication"
    due = (latest if published else timestamp(pr_date)) + INTERVAL
    if now < due:
        return f"Release hold-off ends at {due.isoformat()}"
    return None


def ci_passed(runs, jobs):
    if not runs:
        return False
    latest = max(runs, key=lambda r: r["id"])
    return (latest["status"] == "completed" and latest["conclusion"] == "success"
            and REQUIRED_JOBS <= {j["name"] for j in jobs
                                 if j["status"] == "completed" and j["conclusion"] == "success"})


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--merge", action="store_true", help="Perform the eligible merge")
    args = parser.parse_args()
    repository = os.environ.get("GITHUB_REPOSITORY", "codyps/diesel-fastrace")
    if repository != "codyps/diesel-fastrace":
        raise RuntimeError("Automatic releases are only configured for codyps/diesel-fastrace")
    root = f"repos/{repository}"
    branch = api(root)["default_branch"]

    def holdoff(pr):
        releases = pages(f"{root}/releases")
        closed = [p for p in pages(f"{root}/pulls?state=closed&base={branch}")
                  if release_pr(p, repository, branch)]
        return cooldown_reason(releases, closed, datetime.now(timezone.utc),
                               pr["created_at"])

    candidates = [p for p in pages(f"{root}/pulls?state=open&base={branch}")
                  if release_pr(p, repository, branch)]
    if len(candidates) != 1:
        print(f"Expected one release PR; found {len(candidates)}. Nothing merged.")
        return
    pr = api(f"{root}/pulls/{candidates[0]['number']}")
    # Require the repository automation identity as well as its branch prefix.
    if pr["draft"] or pr["user"]["login"] != "github-actions[bot]":
        print("Release PR is a draft or was not created by the release token account")
        return
    files = pages(f"{root}/pulls/{pr['number']}/files")
    if not files or any(f["filename"] not in RELEASE_FILES
                        or f["status"] not in {"modified", "added"} for f in files):
        print("Release PR changes files outside the expected release metadata")
        return
    sha = pr["head"]["sha"]
    runs = pages(f"{root}/actions/workflows/ci.yml/runs?event=workflow_dispatch&head_sha={sha}",
                 "workflow_runs")
    jobs = (pages(f"{root}/actions/runs/{max(runs, key=lambda r: r['id'])['id']}/jobs", "jobs")
            if runs else [])
    if not ci_passed(runs, jobs):
        print("Latest CI run must pass Quality and Test")
        return
    # GITHUB_TOKEN PR events may have an approval-pending duplicate CI run.
    # The explicitly dispatched CI above must succeed on this exact head.
    # GitHub still enforces required checks and branch rules at merge time.
    current = api(f"{root}/pulls/{pr['number']}")
    if (current["state"] != "open" or current["draft"]
            or current["head"]["sha"] != sha or current["base"]["sha"] != pr["base"]["sha"]
            or not release_pr(current, repository, branch)
            or not current["mergeable"]
            or current["mergeable_state"] not in {"clean", "unstable"}):
        print("PR changed or GitHub has not confirmed it is cleanly mergeable")
        return
    reason = holdoff(current)
    if reason:
        print(reason)
        return
    print(f"Eligible release PR #{pr['number']} at {sha}")
    if args.merge:
        result = api(f"{root}/pulls/{pr['number']}/merge", {"sha": sha, "merge_method": "merge"})
        if not result.get("merged"):
            raise RuntimeError(f"GitHub declined the merge: {result.get('message')}")
        print(f"Merged release PR; the workflow will dispatch release-plz to publish {result['sha']}")
    else:
        print("Preview only; pass --merge to merge")


if __name__ == "__main__":
    main()
