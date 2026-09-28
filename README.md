# yas

Terminal multiplexer and experimental Wayland compositor for browsers and AI agents. Nothing to
configure, no required dependencies.

yas keeps terminals running on a server and streams them to any browser. Every action in the
browser is also a CLI subcommand, so scripts and AI agents can start terminals, type into them,
wait for commands, and read output. On Linux, the same server runs a headless Wayland compositor
and streams graphical apps as H.264 or AV1 video, with Display-P3 and HDR support.

**Documentation: [docs.yas.run](https://docs.yas.run)**

## Install

```bash
curl -sf https://yas.run | sh
```

On Windows, run `irm https://yas.run/install.ps1 | iex` in PowerShell. See
[Installing yas](https://docs.yas.run/getting-started/installing) for the GPL x264 build,
supported platforms, and optional dependencies.

## Use

```bash
yas open                           # open the workspace in a browser
yas share                          # print a URL anyone with the passphrase can open
yas remote add box ssh:box         # save a remote; yas installs itself there on first use
yas terminal start htop            # start a terminal and print its ID
yas terminal show 1                # print its visible text
yas learn                          # print the CLI guide for scripts and agents
```

We publish a [computer agent skill](https://yas.run/SKILL.md). See
[yas for agents](https://docs.yas.run/agents) and the
[CLI reference](https://docs.yas.run/reference/cli).

## Contributing

Building from source, running tests, code conventions, and the release process are in
[CONTRIBUTING.md](CONTRIBUTING.md). Hosted services and CI are in [SERVICES.md](SERVICES.md). How
yas works inside is documented under [Internals](https://docs.yas.run/internals), and the docs site
itself lives in [`docs/site`](docs/site).
