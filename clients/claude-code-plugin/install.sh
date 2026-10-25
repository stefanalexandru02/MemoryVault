#!/usr/bin/env bash
# Builds mem-hook, installs it into this plugin's bin/, and prepares ~/.memory.
#
#   ./install.sh                  build with cargo (profile release-small) and install; on Linux the
#                                 binary is linked statically (a glibc-static, "static-pie" binary)
#   MEM_HOOK_STATIC=0 ./install.sh   build dynamically linked instead
#   MEM_HOOK_BIN=/path/mem-hook ./install.sh   install a prebuilt binary instead of building
#   MEMORY_HOME=/other/dir ./install.sh        use another memory home (default ~/.memory)
#   MEM_INSTALL_QUIET=1 ./install.sh           skip the "Next steps" text (connect.sh does that)
set -euo pipefail

here=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
repo=$(cd "$here/../.." && pwd)
memory_home=${MEMORY_HOME:-$HOME/.memory}

if [ -n "${MEM_HOOK_BIN:-}" ]; then
  built=$MEM_HOOK_BIN
else
  command -v cargo >/dev/null || { echo "cargo not found: install Rust, or set MEM_HOOK_BIN to a prebuilt mem-hook" >&2; exit 1; }
  target_dir=$(cd "$repo" && cargo metadata --format-version 1 --no-deps | sed -n 's/.*"target_directory":"\([^"]*\)".*/\1/p')
  if [ "$(uname -s)" = Linux ] && [ "${MEM_HOOK_STATIC:-1}" != 0 ]; then
    # A static binary runs on any Linux machine, whatever glibc it has. crt-static needs an explicit target.
    host=$(rustc -vV | sed -n 's/^host: //p')
    (cd "$repo" && RUSTFLAGS="${RUSTFLAGS:-} -C target-feature=+crt-static" cargo build --profile release-small -p mem-hook --target "$host")
    built=$target_dir/$host/release-small/mem-hook
  else
    (cd "$repo" && cargo build --profile release-small -p mem-hook)
    built=$target_dir/release-small/mem-hook
  fi
fi
[ -x "$built" ] || { echo "mem-hook binary not found at $built" >&2; exit 1; }
mkdir -p "$here/bin"
install -m 0755 "$built" "$here/bin/mem-hook"
echo "installed $here/bin/mem-hook"

umask 077
mkdir -p "$memory_home"
chmod 700 "$memory_home"
mkdir -p "$memory_home/spool" "$memory_home/sessions" "$memory_home/cache"
if [ ! -e "$memory_home/config" ]; then
  cat > "$memory_home/config" <<'CONFIG'
# MemoryVault client config. Keep this file mode 0600.
# Hooks stay disabled until both MEMORY_URL and MEMORY_TOKEN are set.
# Export it from ~/.zshrc or ~/.bashrc so Claude Code can expand ${MEMORY_URL}/${MEMORY_TOKEN}
# in the MCP server entry:   set -a; . ~/.memory/config; set +a
MEMORY_URL=
MEMORY_TOKEN=
# Optional:
# DEVICE_ID=laptop-x1
# CLAUDE_BIN=/usr/local/bin/claude
# MEM_INJECTION=lean                  # lean or full: what a session starts with (see the README)
# MEM_CONTEXT_ROWS=12                 # rows of the observation index in the full block (1-30)
# MEM_CONTEXT_TYPES=decision,bugfix   # limit that index to these observation types
# MEM_FILE_CONTEXT_BUDGET=1200        # tokens per session for notes shown before a file is read; 0 turns it off
CONFIG
  echo "wrote $memory_home/config (fill in MEMORY_URL and MEMORY_TOKEN)"
else
  echo "kept existing $memory_home/config"
fi
chmod 600 "$memory_home/config"

[ "${MEM_INSTALL_QUIET:-0}" = 1 ] && exit 0

cat <<NEXT

Next steps

The quick way, from the repository root:  make connect-local   (or: make connect-remote URL=... TOKEN=...)
That fills in the config, installs the plugin (hooks, skills, MCP server) and allows its tools.

By hand: see the plugin README, "Install". An optional nightly runner (consolidation), with a runner token in
the config:
     0 3 * * *  $here/bin/mem-hook observe --all --max 50
NEXT
