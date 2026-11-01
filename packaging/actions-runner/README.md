# actions-runner

Guest image for GitHub Actions jobs that run in a microVM. See the Dockerfile header for the
two targets.

## What the image holds

- The Actions runner at `/opt/actions-runner`, pinned by version and sha256. Bump
  `RUNNER_VERSION` and `RUNNER_SHA256` together; the digest is in the release notes.
  Node is pinned the same way (`NODE_VERSION` with `NODE_SHA256` from `SHASUMS256.txt`),
  and pnpm to an exact version.
- git, node, pnpm, jq, gh, python3 and a C toolchain.
- systemd as init. The microVM boots it, and the guest agent that talks to the host is installed
  as a unit when the root filesystem is baked.

It holds no credentials. The runner starts with `run.sh --jitconfig`, using a one-job
configuration that arrives with the job, takes that job and exits.

## Making a root filesystem

```sh
docker build --target actions-runner -t actions-runner packaging/actions-runner
cid=$(docker create actions-runner)
docker export "$cid" | (mkdir rootfs && tar -x -C rootfs)
docker rm "$cid"
truncate -s 8G actions-runner.ext4
mkfs.ext4 -F -d rootfs actions-runner.ext4
```

The host-side tooling then installs the guest agent into the image and snapshots it. That step
lives with the runner, not here.

## Egress

Inside the microVM the runner reaches the network only through the host's allowlisting proxy,
through `HTTPS_PROXY`. A workflow that needs a host beyond the allowlist fails at connect time.
