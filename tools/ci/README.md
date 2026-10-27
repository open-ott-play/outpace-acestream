# CI contracts and Ubuntu package preparation

Run the workflow and mirror-helper contracts with:

```sh
python3 -m unittest discover -s tools/ci -v
```

Install the contract dependencies from `requirements.txt` into a virtual
environment first. The mirror tests also need Node.js. They use temporary
fixture files and never modify the host's APT configuration.

Both ffmpeg-dependent CI jobs use the same guarded Ubuntu mirror helper before
installing packages. On GitHub-hosted Linux, it recognizes the runner image's
Azure primary plus HTTPS Ubuntu fallback and removes only the Azure entry.
This avoids spending the test budget on a slow primary mirror. Missing or
custom mirror lists are left unchanged with a diagnostic; self-hosted and
non-Linux runners are untouched. The helper shares the reviewed logic used by
venus-os-ci-toolkit's browser and Python workflows.

The jobs retain their existing package requirements, APT signature checks,
20-minute limits and all Rust/HLS assertions. A package-install failure is
still a failed job, not a successful or skipped test. If installation remains
slow, inspect the mirror-preparation message and the actual APT output before
attributing the delay to application code or retrying the job.
