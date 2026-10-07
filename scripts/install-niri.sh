#!/usr/bin/env bash
set -euo pipefail
task_root="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
task_data="${XDG_DATA_HOME:-$HOME/.local/share}/niri-computer-use"
task_target="${CARGO_TARGET_DIR:-$task_root/target}"
task_profile="${NIRI_BUILD_PROFILE:-release}"
case "$task_profile" in
  release) cargo build --manifest-path "$task_root/Cargo.toml" --locked --release --bin computer-use-linux --bin niri-clipboard ;;
  debug) cargo build --manifest-path "$task_root/Cargo.toml" --locked --bin computer-use-linux --bin niri-clipboard ;;
  *) echo 'NIRI_BUILD_PROFILE must be debug or release' >&2; exit 2 ;;
esac
install -d "$task_data/bin"
for task_binary in computer-use-linux niri-clipboard; do
  task_staging="$(mktemp "$task_data/bin/.${task_binary}.XXXXXX")"
  install -m755 "$task_target/$task_profile/$task_binary" "$task_staging"
  mv -f -- "$task_staging" "$task_data/bin/$task_binary"
done
install -m755 "$task_root/scripts/niri-desktop-session.py" "$task_data/bin/niri-desktop-session.py"
# The native CLI (`computer-use-linux call|tools`) needs no registration; the
# Codex MCP plugin is opt-in.
if [ "${NIRI_REGISTER_CODEX:-0}" = 1 ]; then
  codex plugin marketplace add "$task_root"
  codex plugin add niri-computer-use@niri-local
fi
"$task_data/bin/computer-use-linux" setup
"$task_data/bin/computer-use-linux" doctor
if [ "${NIRI_REGISTER_CODEX:-0}" = 1 ]; then
  echo 'Installed. Start a new Codex conversation to load the MCP tools.'
else
  echo "Installed $task_data/bin/computer-use-linux. Set NIRI_REGISTER_CODEX=1 to also register the Codex MCP plugin."
fi
