// What YAS's workflows share (xmit-dev/ultimator docs/CI.md): plain functions building steps, imported by relative
// path. GitHub's reusable _build-packages.yml and _build-windows.yml are these here.
import type { Step } from "ultimator:ci";

/**
 * A script written indented in a template literal, as it runs: its first and last empty lines and the indentation
 * its lines share removed. `\${` keeps a shell's `${…}` from the template.
 */
export function sh(
  strings: TemplateStringsArray,
  ...values: readonly (string | number | boolean)[]
): string {
  let text = strings[0]!;
  values.forEach((value, i) => {
    text += String(value) + strings[i + 1]!;
  });
  const lines = text
    .replace(/^\n/, "")
    .replace(/\n[ \t]*$/, "")
    .split("\n");
  const indent = Math.min(
    ...lines
      .filter((line) => line.trim() !== "")
      .map((line) => /^ */.exec(line)![0].length),
  );
  return lines.map((line) => line.slice(indent)).join("\n") + "\n";
}

/**
 * Nix with flakes, on PATH for the job's later steps, in place of DeterminateSystems/nix-installer-action and
 * magic-nix-cache-action. A machine that has Nix (crab) keeps its own; a fresh sandbox gets it from Determinate's
 * installer, with flakes on. There is no binary cache beyond cache.nixos.org: a sandbox's first build of the flake's
 * toolchain is cold.
 */
export function nix(): Step {
  return {
    name: "Nix",
    run: sh`
      if ! command -v nix >/dev/null; then
        for profile in /nix/var/nix/profiles/default/etc/profile.d/nix-daemon.sh "$HOME/.nix-profile/etc/profile.d/nix.sh"; do
          if [ -f "$profile" ]; then . "$profile"; break; fi
        done
      fi
      if ! command -v nix >/dev/null; then
        curl --proto '=https' --tlsv1.2 -sSfL https://install.determinate.systems/nix | sh -s -- install --no-confirm
        . /nix/var/nix/profiles/default/etc/profile.d/nix-daemon.sh
      fi
      dirname "$(command -v nix)" >> "$ULTIMATOR_PATH"
      # Ultimator's sandboxes run as root with Nix's sandbox off, so a builder that makes its $HOME leaves
      # /homeless-shelter on the real root, and every later build there refuses to start ("home directory
      # /homeless-shelter exists"), in this job or the next one a pooled sandbox runs. Build sandboxed, as GitHub's
      # runners did (it falls back when the kernel can't), and clear what an earlier build left.
      if [ "$(uname -s)" = Linux ] && [ "$(id -u)" = 0 ]; then
        rm -rf /homeless-shelter
        if [ -f /etc/nix/nix.conf ] && [ ! -L /etc/nix/nix.conf ] && [ -w /etc/nix/nix.conf ]; then
          echo "sandbox = true" >> /etc/nix/nix.conf
        fi
      fi
      nix --version
      nix config show experimental-features | grep -qw flakes || echo "NIX_CONFIG=experimental-features = nix-command flakes" >> "$ULTIMATOR_ENV"
    `,
  };
}

export type PackagesLabel = "linux-x86_64" | "linux-aarch64" | "macos-aarch64";

/** YAS's release tarballs for one platform (bin/build-tarballs, through the flake) into dist/, and on macOS the
 * complete cross-platform product graph's tests. */
export function packages(label: PackagesLabel): Step[] {
  // Bound aggregate Cargo parallelism to the machine's memory: the arm64 machine serializes its Nix jobs.
  const arm = label === "linux-aarch64";
  const steps: Step[] = [
    nix(),
    {
      name: "Build release tarballs",
      env: {
        NIX_CONFIG: `experimental-features = nix-command flakes\nmax-jobs = ${arm ? 1 : 2}\ncores = ${arm ? 2 : 4}\n`,
      },
      run: "./bin/build-tarballs dist",
    },
  ];
  if (label === "macos-aarch64") {
    steps.push({
      name: "Test the complete cross-platform YAS product graph on macOS",
      env: { RUSTFLAGS: "-D warnings" },
      run: sh`
        # Linux-only compositor probes are Cargo examples. Exercise every workspace library, product binary, and
        # integration/unit test here without asking macOS to compile those live Wayland probes.
        nix develop --command bash -lc '
          cd crates/browser &&
          wasm-pack build --target web --release --out-dir pkg &&
          cd ../../js &&
          pnpm install --frozen-lockfile &&
          pnpm --filter @yas-run/core run build &&
          pnpm --filter @yas-run/solid run build &&
          pnpm --filter @yas-run/ui run build &&
          pnpm --filter yas-web run build
        '
        nix develop --command cargo test --workspace --lib --bins --tests
        nix develop --command bash -lc 'cd js && pnpm install --frozen-lockfile && pnpm run typecheck && pnpm run test'
      `,
    });
  }
  return steps;
}

/**
 * YAS for Windows x86_64 on crab-win, the Windows 11 ARM64 VM on crab, as GitHub's windows-2025 runner built it: an
 * x86_64 toolchain that runs under Windows' x64 emulation, so the binaries and the tests are x86_64 ones. crab-win
 * needs (pcarrier/sys docs/github-runner-windows.md): Git for Windows (bash), rustup, Visual Studio's x64/x86 build
 * tools (component Microsoft.VisualStudio.Component.VC.Tools.x86.x64, beside the ARM64 ones) and 7-Zip. Node.js is
 * the machine's when it has one (22 or later), else a portable one the job fetches; wasm-pack, pnpm and bun are
 * installed by the job.
 */
export function windowsX64(bunVersion = "1.3.13", nodeVersion = "24.11.0"): Step[] {
  return [
    {
      name: "x86_64 toolchain (emulated)",
      shell: "bash",
      run: sh`
        vswhere="/c/Program Files (x86)/Microsoft Visual Studio/Installer/vswhere.exe"
        if [ -z "$("$vswhere" -latest -products '*' -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 -property installationPath)" ]; then
          echo "::error::crab-win needs Visual Studio's x64/x86 build tools (Microsoft.VisualStudio.Component.VC.Tools.x86.x64)"
          exit 1
        fi
        rustup toolchain install stable-x86_64-pc-windows-msvc --profile minimal --target wasm32-unknown-unknown --force-non-host
        echo "RUSTUP_TOOLCHAIN=stable-x86_64-pc-windows-msvc" >> "$ULTIMATOR_ENV"
      `,
    },
    {
      name: "wasm-pack, pnpm, bun",
      shell: "bash",
      env: { BUN_VERSION: bunVersion, NODE_VERSION: nodeVersion },
      run: sh`
        rustc -vV | grep -Fx "host: x86_64-pc-windows-msvc"
        # Node.js: the machine's when it has one, else a portable x64 build kept beside the runner's jobs.
        if ! command -v node >/dev/null; then
          node_dir="$HOME/.ultimator-ci/tools/node-v\${NODE_VERSION}-win-x64"
          if [ ! -x "$node_dir/node.exe" ]; then
            mkdir -p "$HOME/.ultimator-ci/tools"
            zip="$HOME/.ultimator-ci/tools/node-$$.zip"
            curl -fsSL --retry 3 -o "$zip" "https://nodejs.org/dist/v\${NODE_VERSION}/node-v\${NODE_VERSION}-win-x64.zip"
            (cd "$HOME/.ultimator-ci/tools" && unzip -q -o "$zip")
            rm -f "$zip"
          fi
          export PATH="$node_dir:$PATH"
          cygpath -w "$node_dir" >> "$ULTIMATOR_PATH"
        fi
        command -v wasm-pack >/dev/null || cargo install wasm-pack --locked
        npm install -g pnpm@10 "bun@$BUN_VERSION"
        node --version; pnpm --version; bun --version; wasm-pack --version
      `,
    },
    {
      name: "Build UI",
      shell: "bash",
      run: sh`
        (cd crates/browser && wasm-pack build --target web --release --out-dir pkg)
        cd js
        pnpm install --frozen-lockfile
        pnpm --filter @yas-run/core run build
        pnpm --filter @yas-run/solid run build
        pnpm --filter @yas-run/ui run build
        pnpm --filter yas-web run build
      `,
    },
    {
      name: "Build release binaries",
      shell: "bash",
      run: sh`
        cargo build --release -p yas-cli
        file_info="$(od -An -tx1 -j "$(( $(od -An -tu4 -j 60 -N 4 target/release/yas.exe) + 4 ))" -N 2 target/release/yas.exe | tr -d ' ')"
        [ "$file_info" = "6486" ] || { echo "::error::target/release/yas.exe isn't x86_64 (machine $file_info)"; exit 1; }
      `,
    },
    {
      name: "Test the complete cross-platform YAS product graph",
      shell: "bash",
      run: sh`
        # Linux-only compositor probes are Cargo examples. Exercise every workspace library, product binary, and
        # integration/unit test here without asking Windows to compile those live Wayland probes.
        cargo test --workspace --lib --bins --tests
        cd js
        pnpm run typecheck
        pnpm run test
      `,
    },
  ];
}
