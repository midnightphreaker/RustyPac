#!/usr/bin/env python3
"""Repository-local Forgejo release driver. Python 3.11+, git, bash and Podman."""

import base64
import hashlib
import json
import os
import re
import subprocess
import urllib.error
import urllib.parse
import urllib.request
import uuid
from pathlib import Path


def run(*args, capture=False, **kwargs):
    result = subprocess.run(args, check=True, text=True, capture_output=capture, **kwargs)
    return result.stdout.strip() if capture else None


def git(*args):
    return run("git", *args, capture=True)


def container_url(server, repository, target):
    owner, name = repository.split("/", 1)
    owner, name, tag = (
        urllib.parse.quote(part, safe="") for part in (owner, name.lower(), "v" + target)
    )
    return f"{server.rstrip('/')}/{owner}/-/packages/container/{name}/{tag}"


def version(value):
    match = re.fullmatch(
        r"(0|[1-9]\d*)\.(0|[1-9]\d*)\.(0|[1-9]\d*)(?:-[0-9A-Za-z]+(?:[.-][0-9A-Za-z]+)*)?", value
    )
    if not match:
        raise ValueError(f"Expected X.Y.Z or X.Y.Z-prerelease version, got {value!r}")
    return tuple(map(int, match.groups()))


def bump(value, kind):
    a, b, c = version(value)
    if kind == "Major":
        return f"{a}.{b + 1}.0"
    if kind == "minor":
        return f"{a}.{b}.{c + 1}"
    if kind == "retry":
        return value
    raise ValueError("Bump must be Major, minor or retry")


def sync_versions(config, target):
    version(target)
    paths = ["VERSION"]
    Path("VERSION").write_text(target + "\n", encoding="utf-8")
    for spec in config.get("version_files", []):
        path = Path(spec["path"])
        source = path.read_text(encoding="utf-8")
        matches = list(re.finditer(spec["pattern"], source, re.MULTILINE))
        expected = spec.get("count", 1)
        if len(matches) != expected:
            raise ValueError(f"{path}: expected {expected} version matches, got {len(matches)}")
        for match in reversed(matches):
            start, end = match.span("version")
            source = source[:start] + target + source[end:]
        path.write_text(source, encoding="utf-8")
        paths.append(str(path))
    return paths


def pushed_version(config, before):
    """Detect VERSION or an explicitly declared application manifest version change."""
    current = Path("VERSION").read_text().strip()
    candidates = set()
    specs = [{"path": "VERSION", "pattern": r"^(?P<version>\S+)\s*$"}]
    specs += config.get("version_files", [])
    if not before or not re.fullmatch(r"[0-9a-f]{40,64}", before) or set(before) == {"0"}:
        return current
    for spec in specs:
        # Use named group explicitly; patterns may also contain non-version groups.
        now = [
            m.group("version")
            for m in re.finditer(spec["pattern"], Path(spec["path"]).read_text(), re.MULTILINE)
        ]
        try:
            old_text = git("show", f"{before}:{spec['path']}")
        except subprocess.CalledProcessError:
            old_text = ""
        old = [m.group("version") for m in re.finditer(spec["pattern"], old_text, re.MULTILINE)]
        if old != now:
            candidates.update(now)
    if not candidates:
        return None
    if len(candidates) != 1:
        raise ValueError(f"Conflicting application version changes: {sorted(candidates)}")
    target = candidates.pop()
    version(target)
    return target


class Forgejo:
    def __init__(self, server, repository, token):
        self.base = server.rstrip("/") + "/api/v1/repos/" + repository
        self.token = token
        self.server = server.rstrip("/")

    def download(self, asset):
        url = asset["browser_download_url"]
        if urllib.parse.urlsplit(url)[:2] != urllib.parse.urlsplit(self.server)[:2]:
            raise RuntimeError("Release asset URL is outside the configured Forgejo server")
        req = urllib.request.Request(url, headers={"Authorization": "token " + self.token})
        with urllib.request.urlopen(req, timeout=180) as response:
            content = response.read()
        if len(content) != asset["size"]:
            raise RuntimeError("Downloaded release asset size does not match metadata")
        return content

    def request(
        self, method, path, data=None, content_type="application/json", missing=False, raw=False
    ):
        if isinstance(data, dict):
            data = json.dumps(data).encode()
        req = urllib.request.Request(
            self.base + path,
            data=data,
            method=method,
            headers={"Authorization": "token " + self.token, "Content-Type": content_type},
        )
        try:
            with urllib.request.urlopen(req, timeout=180) as response:
                body = response.read()
                return body if raw else json.loads(body) if body else None
        except urllib.error.HTTPError as error:
            if missing and error.code == 404:
                return None
            # Do not echo response bodies, headers, or credential-bearing request objects.
            raise RuntimeError(f"Forgejo {method} {path}: HTTP {error.code}") from None

    def upload(self, release_id, path, provenance, persist):
        route = f"/releases/{release_id}/assets"
        existing = self.request("GET", route)
        payload = path.read_bytes()
        for asset in existing:
            if asset["name"] == path.name:
                expected = provenance["assets"].get(path.name)
                if not expected:
                    raise RuntimeError(f"Existing artifact has no recorded provenance: {path.name}")
                if hashlib.sha256(self.download(asset)).hexdigest() != expected:
                    raise RuntimeError(f"Existing artifact digest mismatch: {path.name}")
                print(f"Verified previously uploaded artifact: {path.name}")
                return
        # Save before uploading: even a lost upload response can be safely retried.
        provenance["assets"][path.name] = hashlib.sha256(payload).hexdigest()
        persist()
        boundary = "release-" + uuid.uuid4().hex
        body = (
            f'--{boundary}\r\nContent-Disposition: form-data; name="attachment"; '
            f'filename="{path.name}"\r\nContent-Type: application/octet-stream\r\n\r\n'
        ).encode()
        body += payload + f"\r\n--{boundary}--\r\n".encode()
        asset = self.request(
            "POST",
            route + "?name=" + urllib.parse.quote(path.name),
            body,
            "multipart/form-data; boundary=" + boundary,
        )
        if asset.get("size") != len(payload):
            raise RuntimeError(f"Uploaded asset size mismatch: {path.name}")
        if hashlib.sha256(self.download(asset)).digest() != hashlib.sha256(payload).digest():
            raise RuntimeError(f"Uploaded asset digest mismatch: {path.name}")


PROVENANCE_PREFIX = "<!-- forgejo-release-provenance:"


def release_body(notes, provenance):
    return notes + "\n\n" + PROVENANCE_PREFIX + json.dumps(provenance, sort_keys=True) + " -->"


def read_provenance(release, tag, commit):
    body = release.get("body", "") or ""
    matches = re.findall(re.escape(PROVENANCE_PREFIX) + r"(.*?) -->", body, re.DOTALL)
    if len(matches) != 1:
        raise RuntimeError("Existing release has missing or ambiguous provenance")
    record = json.loads(matches[0])
    if record.get("schema") != 1 or record.get("tag") != tag or record.get("commit") != commit:
        raise RuntimeError("Release provenance does not match the built tag and commit")
    if not isinstance(record.get("assets"), dict):
        raise RuntimeError("Invalid release asset provenance")
    return record


def save_provenance(api, release_id, notes, provenance):
    route = f"/releases/{release_id}"
    api.request("PATCH", route, {"body": release_body(notes, provenance)})
    observed = read_provenance(api.request("GET", route), provenance["tag"], provenance["commit"])
    if observed != provenance:
        raise RuntimeError("Release provenance readback mismatch")


class Registry:
    """Read exact registry manifests; only a confirmed HTTP404 means absent."""

    def __init__(self, server, user, token):
        self.server = server.rstrip("/")
        credentials = base64.b64encode((user + ":" + token).encode()).decode()
        self.basic_authorization = "Basic " + credentials
        self.authorization = self.basic_authorization

    def manifest(self, image):
        host, path = image.split("/", 1)
        if host != urllib.parse.urlsplit(self.server).netloc:
            raise RuntimeError("Registry image is outside configured Forgejo server")
        repository, reference = path.rsplit("@", 1) if "@" in path else path.rsplit(":", 1)
        url = self.server + "/v2/" + repository + "/manifests/" + reference
        accept = ", ".join(
            (
                "application/vnd.docker.distribution.manifest.v2+json",
                "application/vnd.oci.image.manifest.v1+json",
            )
        )
        for attempt in range(2):
            req = urllib.request.Request(
                url,
                headers={
                    "Authorization": self.authorization,
                    "Accept": accept,
                },
            )
            try:
                with urllib.request.urlopen(req, timeout=180) as response:
                    payload = response.read()
                    declared = response.headers.get("Docker-Content-Digest")
                digest = "sha256:" + hashlib.sha256(payload).hexdigest()
                if declared and declared != digest:
                    raise RuntimeError("Registry manifest digest mismatch")
                manifest = json.loads(payload)
                config_digest = manifest.get("config", {}).get("digest", "")
                if not re.fullmatch(r"sha256:[0-9a-f]{64}", config_digest):
                    raise RuntimeError("Registry image must be a single-platform image manifest")
                return {"digest": digest, "config_digest": config_digest}
            except urllib.error.HTTPError as error:
                if error.code == 404:
                    return None
                if error.code != 401 or attempt:
                    raise RuntimeError(
                        f"Registry manifest request failed: HTTP {error.code}"
                    ) from None
                challenge = error.headers.get("WWW-Authenticate", "")
                if not challenge.lower().startswith("bearer "):
                    raise RuntimeError("Registry rejected credentials") from None
                params = dict(re.findall(r'([a-z]+)="([^"]*)"', challenge))
                realm = params.get("realm", "")
                if urllib.parse.urlsplit(realm)[:2] != urllib.parse.urlsplit(self.server)[:2]:
                    raise RuntimeError(
                        "Registry token endpoint is outside Forgejo server"
                    ) from None
                query = urllib.parse.urlencode(
                    {
                        "service": params.get("service", host),
                        "scope": "repository:" + repository + ":pull",
                    }
                )
                token_url = realm + ("&" if "?" in realm else "?") + query
                token_req = urllib.request.Request(
                    token_url,
                    headers={
                        "Authorization": self.basic_authorization,
                    },
                )
                try:
                    with urllib.request.urlopen(token_req, timeout=180) as response:
                        token_body = json.loads(response.read())
                except urllib.error.HTTPError as token_error:
                    raise RuntimeError(
                        f"Registry authentication failed: HTTP {token_error.code}"
                    ) from None
                token = token_body.get("token") or token_body.get("access_token")
                if not isinstance(token, str) or not token:
                    raise RuntimeError("Registry authentication returned no token") from None
                self.authorization = "Bearer " + token
        raise RuntimeError("Registry authentication did not complete")


def push_verified_image(registry, image, digest_field, expected_digest):
    """A client error is recoverable only when registry content proves the push."""
    push_error = None
    try:
        run("podman", "push", image)
    except subprocess.CalledProcessError as error:
        push_error = error
    observed = registry.manifest(image)
    if not observed or observed[digest_field] != expected_digest:
        raise RuntimeError("Registry readback does not match intended image") from push_error
    if push_error:
        print(
            f"Push client exited {push_error.returncode}; registry image identity verified: {image}"
        )
    return observed


def publish_version_image(registry, image_tag, provenance, persist):
    """Preserve the original image even when its push response/provenance save was lost."""
    existing = registry.manifest(image_tag)
    record = provenance.get("image")
    if existing:
        if not record or existing["config_digest"] != record.get("config_digest"):
            raise RuntimeError("Existing version image does not match recorded build provenance")
        if record.get("digest") and existing["digest"] != record["digest"]:
            raise RuntimeError("Existing version image manifest digest changed")
    else:
        if record and record.get("digest"):
            raise RuntimeError("Previously published version image is missing")
        config_digest = run(
            "podman", "image", "inspect", "--format", "{{.Id}}", image_tag, capture=True
        )
        if not config_digest.startswith("sha256:"):
            config_digest = "sha256:" + config_digest
        if not re.fullmatch(r"sha256:[0-9a-f]{64}", config_digest):
            raise RuntimeError("Local image returned an invalid configuration digest")
        provenance["image"] = {"config_digest": config_digest, "digest": None}
        persist()
        existing = push_verified_image(registry, image_tag, "config_digest", config_digest)
    provenance["image"]["digest"] = existing["digest"]
    persist()
    return existing["digest"]


def publish_latest_image(registry, image, version_digest):
    # Pull the proven original by digest; a retry's locally rebuilt tag may differ.
    pinned = image + "@" + version_digest
    run("podman", "pull", pinned)
    run("podman", "tag", pinned, image + ":latest")
    push_verified_image(registry, image + ":latest", "digest", version_digest)


def assert_remote_head(branch, expected):
    actual = git("ls-remote", "origin", "refs/heads/" + branch).split()[0]
    if actual != expected:
        raise RuntimeError("Default branch advanced; rerun on its current HEAD before publishing")


def commands(items):
    for command in items:
        run("bash", "-euo", "pipefail", "-c", command)


def expected_artifacts(config):
    paths = [Path(os.path.expandvars(p)) for p in config.get("artifacts", [])]
    if len({p.name for p in paths}) != len(paths):
        raise RuntimeError("Artifact filenames must be unique")
    for path in paths:
        if not path.is_file() or path.stat().st_size == 0:
            raise RuntimeError(f"Missing or empty required artifact: {path}")
    return paths


def resume_head(branch, start, marker):
    """A rerun may advance only to its own immediate version commit."""
    git("fetch", "origin", "refs/heads/" + branch)
    remote = git("rev-parse", "FETCH_HEAD")
    if remote != start:
        if (
            marker not in git("log", "-1", "--format=%B", remote)
            or git("rev-parse", remote + "^") != start
        ):
            raise RuntimeError("Default branch advanced outside this run; rerun on current HEAD")
        git("checkout", "--detach", remote)
    return remote


def main():
    config = json.loads(Path(".forgejo/release.json").read_text())
    event = json.loads(Path(os.environ["GITHUB_EVENT_PATH"]).read_text())
    branch = event["repository"]["default_branch"]
    if os.environ["GITHUB_REF"] != "refs/heads/" + branch:
        raise RuntimeError("Releases run only from the default branch")
    start = git("rev-parse", "HEAD")
    if git("status", "--porcelain", "--untracked-files=no"):
        raise RuntimeError("Release checkout must be clean")
    run_id = os.environ["GITHUB_RUN_ID"]
    marker = f"Release automation run {run_id}."
    start = resume_head(branch, start, marker)
    current = Path("VERSION").read_text().strip()
    automatic = os.environ["GITHUB_EVENT_NAME"] == "push"
    if automatic:
        # The version commit's originating run owns publication, including failed retries.
        last_message = git("log", "-1", "--format=%B")
        if "- Release automation run " in last_message and marker not in last_message:
            print("Automation version commit: publication belongs to its originating run")
            return
        target = pushed_version(config, event.get("before"))
        if target is None:
            print("No application version change; no release")
            return
    else:
        kind = event.get("inputs", {}).get("bump", "minor")
        target = current if marker in git("log", "-1", "--format=%B") else bump(current, kind)
    version(target)
    os.environ["RELEASE_VERSION"] = target
    os.environ["RELEASE_NAME"] = event["repository"]["name"]
    os.environ["RELEASE_DIR"] = str(Path(".release-dist").resolve())
    Path(".release-dist").mkdir(exist_ok=True)
    if any(Path(".release-dist").iterdir()):
        raise RuntimeError("Artifact output directory must be empty")
    paths = sync_versions(config, target)
    commands(config.get("checks", []))
    commands(config.get("build", []))
    artifacts = expected_artifacts(config)
    repository = os.environ["GITHUB_REPOSITORY"]
    server = os.environ["GITHUB_SERVER_URL"].rstrip("/")
    image = urllib.parse.urlparse(server).netloc + "/" + repository.lower()
    container = config.get("container")
    image_tag = image + ":v" + target
    os.environ["RELEASE_IMAGE"] = image_tag
    if container:
        run(
            "podman",
            "build",
            "--format",
            "docker",
            "--label",
            "org.opencontainers.image.version=" + target,
            "--label",
            "org.opencontainers.image.source=" + server + "/" + repository,
            "-f",
            container.get("dockerfile", "Dockerfile"),
            "-t",
            image_tag,
            container.get("context", "."),
        )
        commands(container.get("checks", []))
    changed = set(git("diff", "--name-only").splitlines())
    if not changed.issubset(set(paths)):
        raise RuntimeError(
            f"Build modified non-version tracked files: {sorted(changed - set(paths))}"
        )
    assert_remote_head(branch, start)
    token = os.environ.get("RELEASE_TOKEN", "")
    if not token:
        raise RuntimeError(
            "RELEASE_TOKEN is required for repository, release and package publication"
        )
    # Credential values stay in the environment/stdin; no token appears in command arguments.
    askpass = Path(os.environ.get("RUNNER_TEMP", ".")) / ("release-askpass-" + run_id + ".sh")
    askpass.write_text(
        '#!/bin/sh\ncase "$1" in *Username*) printf "%s\\n" "$RELEASE_USER" ;; '
        '*) printf "%s\\n" "$RELEASE_TOKEN" ;; esac\n'
    )
    askpass.chmod(0o700)
    os.environ["GIT_ASKPASS"] = str(askpass.resolve())
    os.environ["GIT_TERMINAL_PROMPT"] = "0"
    os.environ.setdefault("RELEASE_USER", os.environ.get("GITHUB_ACTOR", "release-bot"))
    git("config", "credential.helper", "")
    if changed:
        git("config", "user.name", "Midnight Phreaker")
        git("config", "user.email", "midnightphreaker@gmail.com")
        git("add", "--", *paths)
        message = (
            f"chore(release): prepare v{target}\n\n- Synchronize application version to {target}.\n"
            f"- {marker}\n\n~ Midnight Phreaker ~ <midnightphreaker@gmail.com>"
        )
        git("commit", "-m", message)
        git("push", "origin", "HEAD:refs/heads/" + branch)
    commit = git("rev-parse", "HEAD")
    assert_remote_head(branch, commit)
    api = Forgejo(server, repository, token)
    tag = "v" + target
    existing_tag = api.request("GET", "/tags/" + tag, missing=True)
    if existing_tag and existing_tag["commit"]["sha"] != commit:
        raise RuntimeError(
            f"Tag {tag} already belongs to a different commit; version reuse refused"
        )
    releases = api.request("GET", "/releases?limit=100")
    previous = next((r for r in releases if not r.get("draft") and r["tag_name"] != tag), None)
    revision = commit
    if previous and previous["tag_name"] != tag:
        revision = previous["tag_name"] + ".." + commit
    subjects = git("log", "-10", "--format=%s", revision).splitlines()
    notes = "\n".join("- " + subject for subject in subjects)
    if container:
        notes += f"\n\nContainer: `{image_tag}`\n\n"
        notes += f"[Container image]({container_url(server, repository, target)})\n"
    release = api.request("GET", "/releases/tags/" + tag, missing=True)
    if release is None:
        provenance = {"schema": 1, "tag": tag, "commit": commit, "assets": {}, "image": None}
        release = api.request(
            "POST",
            "/releases",
            {
                "tag_name": tag,
                "target_commitish": commit,
                "name": tag,
                "body": release_body(notes, provenance),
                "draft": True,
                "prerelease": "-" in target,
            },
        )
        release = api.request("GET", "/releases/" + str(release["id"]))
    provenance = read_provenance(release, tag, commit)

    def persist():
        save_provenance(api, release["id"], notes, provenance)

    # Forgejo creates a new tag only when the draft is published.
    resolved_tag = api.request("GET", "/tags/" + tag, missing=True)
    if resolved_tag and resolved_tag["commit"]["sha"] != commit:
        raise RuntimeError(f"Release tag {tag} does not point to the built commit")
    if not resolved_tag and (not release.get("draft") or release.get("target_commitish") != commit):
        raise RuntimeError("Draft release target does not match the built commit")
    for artifact in artifacts:
        api.upload(release["id"], artifact, provenance, persist)
    uploaded = api.request("GET", f"/releases/{release['id']}/assets")
    if {a["name"] for a in uploaded} != {p.name for p in artifacts}:
        raise RuntimeError("Release attachment set does not match required artifacts")
    assert_remote_head(branch, commit)
    if container:
        registry = urllib.parse.urlparse(server).netloc
        run(
            "podman",
            "login",
            registry,
            "--username",
            os.environ["RELEASE_USER"],
            "--password-stdin",
            input=token,
        )
        registry_api = Registry(server, os.environ["RELEASE_USER"], token)
        version_digest = publish_version_image(registry_api, image_tag, provenance, persist)
    api.request(
        "PATCH",
        "/releases/" + str(release["id"]),
        {
            "draft": False,
            "body": release_body(notes, provenance),
        },
    )
    published_tag = api.request("GET", "/tags/" + tag)
    if published_tag["commit"]["sha"] != commit:
        raise RuntimeError("Published release tag does not match the built commit")
    if container:
        assert_remote_head(branch, commit)
        publish_latest_image(registry_api, image, version_digest)
    print(f"Published {server}/{repository}/releases/tag/{tag} at {commit}")


if __name__ == "__main__":
    main()
