#!/usr/bin/env bash
# Connects (or disconnects) this machine's Claude Code, Codex CLI or Gemini CLI to a MemoryVault server.
#
#   connect.sh connect --url URL [--token TOKEN] [--device ID] [--injection lean|full] [--client claude|codex|gemini]
#       builds mem-hook, writes ~/.memory/config, installs the hook plugin, registers the MCP server and
#       pre-approves its tools. Without --token the token already in ~/.memory/config is kept. --injection
#       writes MEM_INJECTION: "full" is the budgeted context block at session start, "lean" only the profile,
#       open work and a short rubric for the search tools; without it the setting in the config (or, if there
#       is none, the per-user default on the server) stays. --client picks the agent CLI (default claude):
#       codex and gemini register the MCP server and the `mem-hook event --client ...` hooks in that tool's own
#       config file (see clients/README.md).
#   connect.sh disconnect [--client claude|codex|gemini] [--purge]
#       undoes all of that and blanks the URL and token in ~/.memory/config; --purge deletes ~/.memory too
#       (including undelivered events in ~/.memory/spool). For codex and gemini it removes exactly the entries
#       connect added and leaves ~/.memory alone.
#   connect.sh probe [--url URL] [--token TOKEN]
#       exit 0 when the token authenticates against the server (defaults come from ~/.memory/config)
#   connect.sh status
#       what is configured and whether it works, plus the tokens served by recent context and recall calls
#
# Everything is idempotent. Settings touched for Claude Code: ~/.claude.json (MCP server "memory", user scope),
# ~/.claude/settings.json (plugin, marketplace and the allow rule for the plugin's MCP tools, through the
# claude CLI and a JSON edit), the plugin's own settings (URL in settings.json, token in the keychain or
# ~/.claude/.credentials.json) and ~/.memory/config. Codex CLI: one marked block in ~/.codex/config.toml. Gemini
# CLI: the mcpServers.memory key and the hooks named "memoryvault" in ~/.gemini/settings.json. A backup of those
# files is kept next to them (*.memoryvault.bak) from the first change until disconnect. Set MEMORY_HOME to use
# another memory home.
set -euo pipefail

here=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
repo=$(cd "$here/../.." && pwd)
memory_home=${MEMORY_HOME:-$HOME/.memory}
config=$memory_home/config
claude_settings=${CLAUDE_CONFIG_DIR:-$HOME/.claude}/settings.json
codex_config=${CODEX_HOME:-$HOME/.codex}/config.toml
gemini_settings=${GEMINI_CLI_HOME:-$HOME}/.gemini/settings.json
codex_begin='# >>> memoryvault (managed by connect.sh; disconnect removes this block) >>>'
codex_end='# <<< memoryvault <<<'
marketplace=memoryvault
plugin=memory
# The plugin's MCP server is mcp__plugin_<plugin>_<server>. The legacy rule belongs to the user-scope entry
# that earlier versions of this script (and the README's manual steps) registered.
allow_rule='mcp__plugin_memory_memory__*'
legacy_allow_rule='mcp__memory__*'

die() { echo "error: $*" >&2; exit 1; }
note() { echo "  $*"; }
have() { command -v "$1" >/dev/null 2>&1; }
scratch=""
trap 'rm -f "$scratch"' EXIT

# Value of KEY in the config file (the last uncommented assignment), or nothing.
cfg_get() { [ -f "$config" ] && sed -n "s/^$1=//p" "$config" | tail -n 1 || true; }

# Sets KEY=VALUE in the config file: replaces an existing assignment, else appends one. Mode stays 0600.
cfg_set() {
  local key=$1 value=$2 tmp
  mkdir -p "$memory_home"
  chmod 700 "$memory_home"
  touch "$config"
  chmod 600 "$config"
  tmp=$(mktemp "$memory_home/config.XXXXXX")
  if grep -q "^$key=" "$config"; then
    V=$value awk -v k="$key" 'index($0, k "=") == 1 { print k "=" ENVIRON["V"]; next } { print }' "$config" > "$tmp"
  else
    { cat "$config"; printf '%s=%s\n' "$key" "$value"; } > "$tmp"
  fi
  chmod 600 "$tmp"
  mv "$tmp" "$config"
}

# HTTP status of GET $1/v1/context with bearer $2 (000 when the server cannot be reached).
http_status() { curl -s -o /dev/null -m 10 -w '%{http_code}' -H "Authorization: Bearer $2" "$1/v1/context" || true; }

# 0: the token works; 1: the server rejected it; 2: the server could not be asked.
token_check() {
  case $(http_status "$1" "$2") in
    2??|3??) return 0 ;;
    401|403) return 1 ;;
    *) return 2 ;;
  esac
}

normalize_url() {
  local u=${1%/}
  case $u in http://*|https://*) printf '%s' "$u" ;; *) die "the URL must start with http:// or https:// (got '$1')" ;; esac
}

# Adds ($1 = add) or removes ($1 = remove) the allow rule in the Claude Code user settings. Other keys are kept.
# Exit 0 done, 3 the file is not the JSON this expects, 4 neither python3 nor jq is installed.
edit_allow_rule() {
  local action=$1 allow_rule=$2 tmp
  mkdir -p "$(dirname "$claude_settings")"
  if have python3; then
    python3 -I - "$claude_settings" "$action" "$allow_rule" <<'PY'
import json, os, sys, tempfile
path, action, rule = sys.argv[1:4]
try:
    data = json.load(open(path)) if os.path.exists(path) and os.path.getsize(path) else {}
    if not isinstance(data, dict):
        raise ValueError
    perms = data.get("permissions", {})
    if not isinstance(perms, dict):
        raise ValueError
    allow = perms.get("allow", [])
    if not isinstance(allow, list):
        raise ValueError
except ValueError:
    sys.exit(3)
if action == "add" and rule not in allow:
    data.setdefault("permissions", perms)["allow"] = allow + [rule]
elif action == "remove" and rule in allow:
    allow.remove(rule)
else:
    sys.exit(0)
fd, tmp = tempfile.mkstemp(dir=os.path.dirname(path), prefix=".settings.")
with os.fdopen(fd, "w") as f:
    json.dump(data, f, indent=2)
    f.write("\n")
if os.path.exists(path):
    os.chmod(tmp, os.stat(path).st_mode & 0o777)
os.replace(tmp, path)
PY
  elif have jq; then
    [ -s "$claude_settings" ] || echo '{}' > "$claude_settings"
    tmp=$(mktemp "$(dirname "$claude_settings")/.settings.XXXXXX")
    if [ "$action" = add ]; then
      jq --arg r "$allow_rule" '.permissions.allow = ((.permissions.allow // []) + [$r] | unique)' "$claude_settings" > "$tmp" || { rm -f "$tmp"; return 3; }
    else
      jq --arg r "$allow_rule" 'if .permissions.allow then .permissions.allow -= [$r] else . end' "$claude_settings" > "$tmp" || { rm -f "$tmp"; return 3; }
    fi
    mv "$tmp" "$claude_settings"
  else
    return 4
  fi
}

allow_rule_step() {
  local action=$1 allow_rule=$2 rc=0
  edit_allow_rule "$action" "$allow_rule" || rc=$?
  case $rc in
    0) if [ "$action" = add ]; then note "allowed $allow_rule without prompts"; else note "cleared the $allow_rule allow rule"; fi ;;
    3) echo "warning: $claude_settings is not the JSON object I expected; edit it by hand: permissions.allow $action \"$allow_rule\"" >&2 ;;
    *) echo "warning: python3 or jq is needed to edit $claude_settings; edit it by hand: permissions.allow $action \"$allow_rule\"" >&2 ;;
  esac
}

# --- Codex CLI and Gemini CLI ---------------------------------------------------------------------------------
# Both get the MCP server and the hooks that feed `mem-hook event --client <name>`. Each tool's own config file is
# only touched through the entries listed in the header, after a backup, and not at all when it cannot be parsed.

# The hook command line; it sits inside quotes in TOML and JSON, so characters the shell would expand are refused.
hook_command() {
  case $here in *[\"\'\`\$\\]*) die "the checkout path must not contain quotes, backticks, \$ or backslashes (got '$here')" ;; esac
  printf '"%s/bin/mem-hook" event --client %s' "$here" "$1"
}

# Adds ($1 = add) or removes ($1 = remove) our entries in the Gemini CLI user settings. Prints "mcp-skipped" when
# a server named "memory" that is not ours is already there (it is left alone). Exit 0 done, 3 the file is not
# the JSON object this expects (nothing is written), 4 python3 is not installed.
edit_gemini_settings() {
  local action=$1 mcp_url=${2:-} token=${3:-} command=${4:-}
  have python3 || return 4
  mkdir -p "$(dirname "$gemini_settings")"
  python3 -I - "$gemini_settings" "$action" "$mcp_url" "$token" "$command" <<'PY'
import json, os, shutil, sys, tempfile
path, action, mcp_url, token, command = sys.argv[1:6]
NAME = "memoryvault"
# (event, timeout in ms): Gemini runs hooks synchronously, so the budgets are short.
EVENTS = [("SessionStart", 10000), ("BeforeAgent", 5000), ("AfterTool", 5000), ("AfterAgent", 5000), ("SessionEnd", 2000)]
backup = path + ".memoryvault.bak"

def ours_server(v):
    headers = v.get("headers") if isinstance(v, dict) else None
    auth = headers.get("Authorization") if isinstance(headers, dict) else None
    return isinstance(auth, str) and auth.startswith("Bearer mem_") and str(v.get("httpUrl", "")).endswith("/mcp")

def ours_hook(h):
    return isinstance(h, dict) and h.get("name") == NAME

def strip_hooks(hooks):
    """Drops our hooks (and the groups and events that held only them); returns whether anything went."""
    changed = False
    for event in list(hooks):
        groups = hooks[event]
        if not isinstance(groups, list):
            continue
        kept = []
        for g in groups:
            inner = g.get("hooks") if isinstance(g, dict) else None
            if isinstance(inner, list) and any(ours_hook(h) for h in inner):
                changed = True
                inner = [h for h in inner if not ours_hook(h)]
                if not inner:
                    continue
                g = dict(g, hooks=inner)
            kept.append(g)
        if len(kept) != len(groups):
            if kept:
                hooks[event] = kept
            else:
                del hooks[event]
    return changed

try:
    raw = open(path).read() if os.path.exists(path) else ""
    data = json.loads(raw) if raw.strip() else {}
    servers = data.get("mcpServers", {})
    hooks = data.get("hooks", {})
    if not isinstance(servers, dict) or not isinstance(hooks, dict):
        raise ValueError
except (ValueError, AttributeError):
    sys.exit(3)

existed = os.path.exists(path)
had_ours = ours_server(servers.get("memory")) or any(
    ours_hook(h) for gs in hooks.values() if isinstance(gs, list)
    for g in gs if isinstance(g, dict) and isinstance(g.get("hooks"), list) for h in g["hooks"])
emptied_hooks = bool(hooks)
removed_hooks = strip_hooks(hooks)
if action == "add":
    if existed and not had_ours:
        shutil.copy2(path, backup)
    if "memory" in servers and not ours_server(servers["memory"]):
        print("mcp-skipped")
    else:
        servers["memory"] = {"httpUrl": mcp_url, "headers": {"Authorization": "Bearer " + token}}
        data["mcpServers"] = servers
    for event, timeout in EVENTS:
        group = {"hooks": [{"name": NAME, "type": "command", "command": command, "timeout": timeout}]}
        if event == "AfterTool":
            group["matcher"] = "*"
        hooks.setdefault(event, []).append(group)
    data["hooks"] = hooks
else:
    if ours_server(servers.get("memory")):
        del servers["memory"]
        if not servers:
            del data["mcpServers"]
    if removed_hooks and not hooks and emptied_hooks:
        del data["hooks"]
    if not had_ours:
        sys.exit(0)
    if os.path.exists(backup):
        if json.load(open(backup)) == data:
            shutil.copy2(backup, path)
            os.remove(backup)
            sys.exit(0)
        os.remove(backup)
    if not data:
        os.remove(path)
        sys.exit(0)

fd, tmp = tempfile.mkstemp(dir=os.path.dirname(path), prefix=".settings.")
with os.fdopen(fd, "w") as f:
    json.dump(data, f, indent=2)
    f.write("\n")
os.chmod(tmp, os.stat(path).st_mode & 0o777 if existed else 0o600)
os.replace(tmp, path)
PY
}

gemini_step() {
  local action=$1 rc=0 out
  out=$(edit_gemini_settings "$@") || rc=$?
  case $rc in
    0) ;;
    3) echo "warning: $gemini_settings is not a JSON object I can edit (comments? wrong shape?); nothing was changed there" >&2; return 1 ;;
    *) echo "warning: python3 is needed to edit $gemini_settings; nothing was changed there" >&2; return 1 ;;
  esac
  if [ "$action" = remove ]; then
    note "removed the memory server and the memoryvault hooks from $gemini_settings"
  elif [ "$out" = mcp-skipped ]; then
    echo "warning: $gemini_settings already has an MCP server named 'memory' that is not ours; left it alone" >&2
    note "registered the hooks in $gemini_settings"
  else
    note "registered the MCP server and the hooks in $gemini_settings (the token is stored there)"
  fi
  [ ! -f "$gemini_settings.memoryvault.bak" ] || note "kept your original as $gemini_settings.memoryvault.bak"
}

# The config file without our marked block (and the blank line that precedes it).
codex_without_block() {
  awk -v b="$codex_begin" -v e="$codex_end" '
    skip { if ($0 == e) skip = 0; next }
    $0 == b { skip = 1; held = 0; next }
    held { print ""; held = 0 }
    $0 == "" { held = 1; next }
    { print }
    END { if (held) print "" }' "$1"
}

# Does $1 (a config file) define the Codex MCP server "memory" itself / any of the hook events we use?
codex_has_memory_server() {
  grep -Eq "^[[:space:]]*(\[mcp_servers\.[\"']?memory[\"']?[].]|mcp_servers[[:space:]]*=|mcp_servers\.[\"']?memory[\"']?[[:space:].=])" "$1"
}
codex_has_own_hooks() {
  local ev='(SessionStart|UserPromptSubmit|PostToolUse|Stop|SessionEnd)'
  grep -Eq "^[[:space:]]*(hooks[[:space:]]*=|hooks\.$ev[[:space:].=]|$ev[[:space:]]*=)" "$1"
}

# 0 when $1 parses as TOML or this machine cannot tell (no python3 with tomllib), 1 when it does not.
toml_ok() {
  have python3 || return 0
  python3 -I -c 'import sys
try:
    import tomllib
except ImportError:
    sys.exit(0)
try:
    tomllib.load(open(sys.argv[1], "rb"))
except Exception:
    sys.exit(1)' "$1"
}

codex_block() {
  local mcp_url=$1 token=$2 with_mcp=$3 with_hooks=$4 command spec name timeout async
  command=$(hook_command codex)
  printf '%s\n' "$codex_begin"
  if [ "$with_mcp" = 1 ]; then
    printf '[mcp_servers.memory]\nurl = "%s"\nhttp_headers = { Authorization = "Bearer %s" }\n' "$mcp_url" "$token"
  fi
  if [ "$with_hooks" = 1 ]; then
    # SessionEnd runs for at most 3 seconds in Codex. Tool and stop events are asynchronous, like the Claude Code ones.
    for spec in SessionStart:10: UserPromptSubmit:5: PostToolUse:5:true Stop:5:true SessionEnd:2:; do
      IFS=: read -r name timeout async <<<"$spec"
      printf '\n[[hooks.%s]]\n[[hooks.%s.hooks]]\ntype = '"'"'command'"'"'\ncommand = '"'"'%s'"'"'\ntimeout = %s\n' \
        "$name" "$name" "$command" "$timeout"
      [ -z "$async" ] || printf 'async = true\n'
    done
  fi
  printf '%s\n' "$codex_end"
}

codex_connect() {
  local mcp_url=$1 token=$2 with_mcp=1 with_hooks=1 new
  hook_command codex >/dev/null
  if [ -f "$codex_config" ]; then
    grep -qxF "$codex_begin" "$codex_config" && ! grep -qxF "$codex_end" "$codex_config" \
      && die "$codex_config has the start marker of our block but not its end; fix or remove it by hand first"
    toml_ok "$codex_config" || die "$codex_config is not valid TOML; fix it first (nothing was changed)"
  fi
  new=$(mktemp "${TMPDIR:-/tmp}/codex-config.XXXXXX")
  scratch=$new
  if [ -f "$codex_config" ]; then
    codex_without_block "$codex_config" > "$new"
  fi
  if codex_has_memory_server "$new"; then
    with_mcp=0
    echo "warning: $codex_config already defines an MCP server 'memory'; left it alone" >&2
  fi
  if codex_has_own_hooks "$new"; then
    with_hooks=0
    echo "warning: $codex_config defines hooks in a form I cannot extend safely; the hooks were not registered" >&2
  fi
  if [ "$with_mcp$with_hooks" = 00 ]; then
    return 0
  fi
  { [ ! -s "$new" ] || printf '\n'; codex_block "$mcp_url" "$token" "$with_mcp" "$with_hooks"; } >> "$new"
  toml_ok "$new" || die "the result would not be valid TOML (is $codex_config unusual?); nothing was changed"
  mkdir -p "$(dirname "$codex_config")"
  if [ -f "$codex_config" ]; then
    grep -qxF "$codex_begin" "$codex_config" || cp -p "$codex_config" "$codex_config.memoryvault.bak"
    cat "$new" > "$codex_config"
  else
    (umask 077; cat "$new" > "$codex_config")
  fi
  [ "$with_mcp" = 0 ] || note "registered the MCP server in $codex_config (the token is stored there)"
  [ "$with_hooks" = 0 ] || note "registered the hooks in $codex_config; Codex runs a new hook only after you approve it there"
  [ ! -f "$codex_config.memoryvault.bak" ] || note "kept your original as $codex_config.memoryvault.bak"
}

codex_disconnect() {
  local new
  [ -f "$codex_config" ] && grep -qxF "$codex_begin" "$codex_config" || { note "nothing of ours in $codex_config"; return 0; }
  grep -qxF "$codex_end" "$codex_config" || die "$codex_config has the start marker of our block but not its end; remove the block by hand"
  new=$(mktemp "${TMPDIR:-/tmp}/codex-config.XXXXXX")
  scratch=$new
  codex_without_block "$codex_config" > "$new"
  if [ -f "$codex_config.memoryvault.bak" ] && [ "$(cat "$new")" = "$(cat "$codex_config.memoryvault.bak")" ]; then
    cat "$codex_config.memoryvault.bak" > "$codex_config"
  elif [ -z "$(tr -d '[:space:]' < "$new")" ] && [ ! -f "$codex_config.memoryvault.bak" ]; then
    rm -f "$codex_config"
  else
    cat "$new" > "$codex_config"
  fi
  rm -f "$codex_config.memoryvault.bak"
  note "removed our block from $codex_config"
}

# Validates a --client value.
check_client() {
  case $1 in claude|codex|gemini) ;; *) die "--client must be claude, codex or gemini (got '$1')" ;; esac
}

client_label() {
  case $1 in claude) echo "Claude Code" ;; codex) echo "Codex CLI" ;; *) echo "Gemini CLI" ;; esac
}

cmd_connect() {
  local url="" token="" device="" injection="" client=claude hook
  while [ $# -gt 0 ]; do
    case $1 in
      --client) client=${2:?--client needs a value}; shift 2 ;;
      --url) url=${2:?--url needs a value}; shift 2 ;;
      --token) token=${2:?--token needs a value}; shift 2 ;;
      --device) device=${2:?--device needs a value}; shift 2 ;;
      --injection) injection=${2:?--injection needs a value}; shift 2 ;;
      *) die "unknown option '$1'" ;;
    esac
  done
  [ -n "$url" ] || die "--url is required"
  case $injection in ""|lean|full) ;; *) die "--injection must be lean or full (got '$injection')" ;; esac
  check_client "$client"
  url=$(normalize_url "$url")
  [ -n "$token" ] || token=$(cfg_get MEMORY_TOKEN)
  [ -n "$token" ] || die "no token: pass --token (or put MEMORY_TOKEN in $config)"
  case $token in mem_*_*) ;; *) die "that does not look like a MemoryVault token (expected mem_<prefix>_<secret>)" ;; esac
  [ -n "$device" ] || device=$(cfg_get DEVICE_ID)
  [ -n "$device" ] || device=$(hostname -s 2>/dev/null || hostname)
  [ "$client" != claude ] || have claude || die "the claude CLI is not on PATH"
  have curl || die "curl is needed to check the server"
  case $url$token in *\"*|*\\*) die "the URL and token must not contain quotes or backslashes" ;; esac

  echo "Connecting $(client_label "$client") to $url as $device"
  local rc=0
  token_check "$url" "$token" || rc=$?
  case $rc in
    0) ;;
    1) die "$url rejected the token (revoked, mistyped, or minted by another server)" ;;
    *) die "cannot reach $url (is the server running, and is the URL right?)" ;;
  esac
  note "server accepts the token"

  MEM_INSTALL_QUIET=1 "$here/install.sh" >/dev/null
  note "built $here/bin/mem-hook"

  cfg_set MEMORY_URL "$url"
  cfg_set MEMORY_TOKEN "$token"
  cfg_set DEVICE_ID "$device"
  [ -z "$injection" ] || cfg_set MEM_INJECTION "$injection"
  note "wrote $config"

  case $client in
    claude) connect_claude "$url" "$token" ;;
    codex) codex_connect "$url/mcp" "$token"; connected_message "$client" ;;
    gemini) hook=$(hook_command gemini); gemini_step add "$url/mcp" "$token" "$hook" || exit 1; connected_message "$client" ;;
  esac
}

connected_message() {
  echo
  echo "Done. Start a new $(client_label "$1") session (running ones keep the old setup)."
  echo "Undo it with:  $here/connect.sh disconnect --client $1"
}

connect_claude() {
  local url=$1 token=$2

  # Plugin: removed first so that a moved checkout or a new version is picked up.
  claude plugin uninstall "$plugin@$marketplace" >/dev/null 2>&1 || true
  claude plugin marketplace remove "$marketplace" >/dev/null 2>&1 || true
  claude plugin marketplace add "$repo" >/dev/null || die "claude plugin marketplace add $repo failed"
  claude plugin install "$plugin@$marketplace" >/dev/null || die "claude plugin install $plugin@$marketplace failed"
  note "installed the plugin $plugin@$marketplace: hooks, skills and the MCP server (loads from $here)"

  # MCP: the plugin bundles the server (.mcp.json) and reads the URL and token from its own settings, so apps
  # that do not read the shell profile work too. The token goes to the keychain, not into ~/.claude.json.
  printf '{"memory_url":"%s","memory_token":"%s"}' "$url" "$token" \
    | claude plugin configure "$plugin@$marketplace" --values-stdin >/dev/null \
    || die "claude plugin configure $plugin@$marketplace failed"
  claude mcp remove -s user memory >/dev/null 2>&1 && note "removed the old user-scope MCP entry 'memory'" || true
  note "configured the plugin's MCP server (token kept out of ~/.claude.json)"

  allow_rule_step remove "$legacy_allow_rule" >/dev/null
  allow_rule_step add "$allow_rule"

  echo
  echo "Done. Start a new Claude Code session (running ones keep the old setup)."
  echo "Check it with:  make connect-status    Undo it with:  make disconnect"
}

cmd_disconnect() {
  local purge=0 client=claude
  while [ $# -gt 0 ]; do
    case $1 in
      --purge) purge=1; shift ;;
      --client) client=${2:?--client needs a value}; shift 2 ;;
      *) die "unknown option '$1'" ;;
    esac
  done
  check_client "$client"
  if [ "$client" != claude ]; then
    # The shared ~/.memory stays: other clients on this machine may still use it.
    echo "Disconnecting $(client_label "$client") from MemoryVault"
    if [ "$client" = codex ]; then codex_disconnect; else gemini_step remove || exit 1; fi
    if [ "$purge" = 1 ]; then
      rm -rf "$memory_home"
      note "deleted $memory_home"
    fi
    echo
    echo "Done."
    return 0
  fi
  echo "Disconnecting Claude Code from MemoryVault"
  if have claude; then
    claude mcp remove -s user memory >/dev/null 2>&1 && note "removed the old user-scope MCP entry 'memory'" || true
    claude plugin uninstall "$plugin@$marketplace" >/dev/null 2>&1 && note "uninstalled the plugin (hooks, skills, MCP server and its saved token)" || note "the plugin was not installed"
    claude plugin marketplace remove "$marketplace" >/dev/null 2>&1 || true
  else
    echo "warning: the claude CLI is not on PATH; the MCP server and the plugin were left alone" >&2
  fi
  if [ -f "$claude_settings" ]; then
    allow_rule_step remove "$allow_rule"
    allow_rule_step remove "$legacy_allow_rule" >/dev/null
  fi
  if [ "$purge" = 1 ]; then
    rm -rf "$memory_home"
    note "deleted $memory_home"
  elif [ -f "$config" ]; then
    cfg_set MEMORY_URL ""
    cfg_set MEMORY_TOKEN ""
    note "blanked MEMORY_URL and MEMORY_TOKEN in $config (spool and sessions kept; --purge deletes them)"
  fi
  echo
  echo "Done."
}

cmd_probe() {
  local url="" token=""
  while [ $# -gt 0 ]; do
    case $1 in
      --url) url=${2:?--url needs a value}; shift 2 ;;
      --token) token=${2:?--token needs a value}; shift 2 ;;
      *) die "unknown option '$1'" ;;
    esac
  done
  local cfg_url
  cfg_url=$(cfg_get MEMORY_URL)
  if [ -z "$token" ]; then
    # A token saved for another server is of no use here.
    [ -z "$url" ] || [ "$url" = "$cfg_url" ] || return 1
    token=$(cfg_get MEMORY_TOKEN)
  fi
  [ -n "$url" ] || url=$cfg_url
  [ -n "$url" ] && [ -n "$token" ] || return 1
  token_check "$(normalize_url "$url")" "$token"
}

# Tokens served by this user's recent context and recall calls (GET $1/v1/stats/recall with bearer $2), one
# line per call kind and injection mode. Prints nothing when the server cannot be asked.
print_recall_stats() {
  local body days rows
  body=$(curl -fsS -m 10 -H "Authorization: Bearer $2" "$1/v1/stats/recall" 2>/dev/null) || return 0
  days=$(printf '%s' "$body" | sed -n 's/.*"days":\([0-9]*\).*/\1/p')
  rows=$(printf '%s' "$body" | sed 's/},{/}\n{/g' | sed -n 's/.*"mode":"\([a-z]*\)","injection":\(null\|"[a-z]*"\),"calls":\([0-9]*\),"avg_tokens":\([0-9.e+-]*\),"p50_tokens":\([0-9.e+-]*\),"p95_tokens":\([0-9.e+-]*\).*/\1 \2 \3 \4 \5 \6/p')
  if [ -z "$rows" ]; then
    echo "tokens   no context or recall calls in the last ${days:-7} days"
    return 0
  fi
  echo "tokens   served per call over the last ${days:-7} days (avg / p50 / p95)"
  printf '%s\n' "$rows" | awk '{ gsub(/"/, "", $2); if ($2 == "null") $2 = "-"
    printf "         %-8s %-5s %5d calls  %6.0f /%6.0f /%6.0f\n", $1, $2, $3, $4, $5, $6 }'
}

cmd_status() {
  local url token
  url=$(cfg_get MEMORY_URL)
  token=$(cfg_get MEMORY_TOKEN)
  if [ -z "$url" ] || [ -z "$token" ]; then
    echo "config   not connected ($config has no MEMORY_URL/MEMORY_TOKEN)"
  else
    echo "config   $url  device $(cfg_get DEVICE_ID)  token ${token%%_*}_$(printf '%s' "$token" | cut -d_ -f2)_..."
    local rc=0
    token_check "$url" "$token" || rc=$?
    case $rc in
      0) echo "server   reachable, token accepted" ;;
      1) echo "server   reachable, but the token was REJECTED (make connect-local or connect-remote again)" ;;
      *) echo "server   not reachable" ;;
    esac
    if [ $rc = 0 ]; then
      print_recall_stats "$url" "$token"
    fi
  fi
  local injection
  injection=$(cfg_get MEM_INJECTION)
  echo "inject   ${injection:-not set in the config: the per-user default on the server applies (full unless changed)}"
  if have claude; then
    if [ "$(claude plugin configure "$plugin@$marketplace" 2>/dev/null | grep -c ', set)')" = 2 ]; then
      echo "mcp      the plugin's MCP server has its URL and token"
    else
      echo "mcp      the plugin's MCP server is not configured"
    fi
    claude plugin list 2>/dev/null | grep -q "$plugin@$marketplace" && echo "plugin   $plugin@$marketplace installed" || echo "plugin   not installed"
  fi
  if [ -f "$claude_settings" ] && grep -q "mcp__plugin_memory_memory__" "$claude_settings"; then
    echo "allow    $allow_rule"
  else
    echo "allow    no allow rule (tool calls will prompt)"
  fi
  [ -x "$here/bin/mem-hook" ] && echo "binary   $here/bin/mem-hook" || echo "binary   $here/bin/mem-hook missing"
  if [ -f "$codex_config" ] && grep -qxF "$codex_begin" "$codex_config"; then
    echo "codex    registered in $codex_config"
  fi
  if [ -f "$gemini_settings" ] && grep -q '"name": "memoryvault"' "$gemini_settings"; then
    echo "gemini   registered in $gemini_settings"
  fi
}

sub=${1:-}
[ $# -eq 0 ] || shift
case $sub in
  connect) cmd_connect "$@" ;;
  disconnect) cmd_disconnect "$@" ;;
  probe) cmd_probe "$@" ;;
  status) cmd_status ;;
  *) sed -n '2,19p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'; [ -z "$sub" ] || [ "$sub" = help ] || exit 2 ;;
esac
