#!/bin/sh
# Sets up meersh from this checkout on a Linux desktop. Run it again at any time. Steps that
# are done already stay as they are, and tools that are not installed are skipped.
#
#   integrations/setup.sh                 terminals, VS Code and Claude Code
#   integrations/setup.sh --login-shell   also makes meersh the login shell, with sudo
#
# The steps are these.
#   - ~/.local/bin/meersh starts the newest build of this checkout, debug or release. If
#     there is no build yet, a release is built first.
#   - Konsole (and Yakuake with it), Ptyxis and VS Code open new terminals with it.
#   - ~/.config/meersh/agents.msh holds the policy for coding agents and init.msh loads it.
#     MEERSH_AGENT_PROGRAMS lists the programs agents may run beyond the template.
#   - Claude Code gets the meersh MCP server for all projects, its tools in the allow list
#     and the section from claude/CLAUDE-meersh-fallback.md in ~/.claude/CLAUDE.md. Bash,
#     Edit and Write stay available. claude/README.md describes how to block them.
#   - --login-shell installs integrations/login-shell as /usr/local/bin/meersh-login/bash,
#     adds it to /etc/shells and makes it your login shell. Terminals then get meersh and
#     everything else gets bash. `sudo chsh -s /bin/bash $USER` undoes it.

set -eu

repo=$(cd "$(dirname "$0")/.." && pwd)
wrapper=$HOME/.local/bin/meersh
config=${XDG_CONFIG_HOME:-$HOME/.config}
data=${XDG_DATA_HOME:-$HOME/.local/share}
claude_dir=${CLAUDE_CONFIG_DIR:-$HOME/.claude}
programs=${MEERSH_AGENT_PROGRAMS:-python python3 pytest curl psql docker make npm node}
login_path=/usr/local/bin/meersh-login/bash

login_shell=no
for arg in "$@"; do
    case $arg in
        --login-shell) login_shell=yes ;;
        -h | --help)
            sed -n '2,/^$/s/^# \{0,1\}//p' "$0"
            exit 0
            ;;
        *)
            echo "unknown option $arg, see $0 --help" >&2
            exit 2
            ;;
    esac
done

step() { printf '\n%s\n' "$*"; }
say() { printf '  %s\n' "$*"; }
has() { command -v "$1" >/dev/null 2>&1; }

step "Build"
if [ -x "$repo/target/release/meersh" ] || [ -x "$repo/target/debug/meersh" ]; then
    say "found a build in $repo/target"
elif has cargo; then
    say "building a release, this takes a few minutes"
    cargo build --release --manifest-path "$repo/Cargo.toml"
else
    echo "There is no build in $repo/target and no cargo. Install Rust from https://rustup.rs and run this again." >&2
    exit 1
fi

step "Wrapper $wrapper"
wrapper_text() {
    cat <<EOF
#!/bin/sh
# Runs the most recently built meersh from the checkout in $repo.
# integrations/setup.sh wrote it. Terminals, VS Code and the Claude Code MCP server start it.
target=$repo/target
bin=\$target/debug/meersh
[ "\$target/release/meersh" -nt "\$bin" ] && bin=\$target/release/meersh
[ -x "\$bin" ] && exec "\$bin" "\$@"
echo "meersh: no build in \$target" >&2
# A terminal still gets a shell, a caller with arguments gets an error.
[ \$# -eq 0 ] && exec /bin/bash
exit 127
EOF
}
mkdir -p "$(dirname "$wrapper")"
if [ -e "$wrapper" ] && ! grep -q 'Runs the most recently built meersh' "$wrapper"; then
    mv "$wrapper" "$wrapper.before-setup"
    say "moved the meersh that was there to $wrapper.before-setup"
fi
if [ "$(cat "$wrapper" 2>/dev/null)" = "$(wrapper_text)" ]; then
    say "unchanged"
else
    wrapper_text >"$wrapper"
    chmod 755 "$wrapper"
    say "written"
fi
case ":$PATH:" in
    *":$HOME/.local/bin:"*) ;;
    *) say "~/.local/bin is not in PATH, so type the full path or add it in ~/.bashrc" ;;
esac

kwrite=$(command -v kwriteconfig6 || command -v kwriteconfig5 || true)
kread=$(command -v kreadconfig6 || command -v kreadconfig5 || true)
if has konsole && [ -n "$kwrite" ] && [ -n "$kread" ]; then
    step "Konsole"
    profile=$("$kread" --file konsolerc --group "Desktop Entry" --key DefaultProfile)
    if [ -z "$profile" ]; then
        profile=meersh.profile
        "$kwrite" --file konsolerc --group "Desktop Entry" --key DefaultProfile "$profile"
        say "made $profile the default profile"
    fi
    file=$data/konsole/$profile
    if [ ! -f "$file" ]; then
        mkdir -p "$data/konsole"
        "$kwrite" --file "$file" --group General --key Name "${profile%.profile}"
        "$kwrite" --file "$file" --group General --key Parent FALLBACK/
    fi
    "$kwrite" --file "$file" --group General --key Command "$wrapper"
    say "the profile $profile runs the wrapper. Yakuake uses the same profile."
fi

if has gsettings && gsettings list-schemas 2>/dev/null | grep -qx org.gnome.Ptyxis; then
    step "Ptyxis"
    uuid=$(gsettings get org.gnome.Ptyxis default-profile-uuid | tr -d "'")
    if [ -n "$uuid" ]; then
        schema=org.gnome.Ptyxis.Profile:/org/gnome/Ptyxis/Profiles/$uuid/
        gsettings set "$schema" custom-command "$wrapper"
        gsettings set "$schema" use-custom-command true
        say "the default profile runs the wrapper"
    else
        say "Ptyxis has no profile yet. Open it once and run this again."
    fi
fi

# Sets the meersh terminal profile in a VS Code settings.json. A file with comments or
# trailing commas is not plain JSON, so it is left alone and the lines to add are shown.
vscode_settings() {
    python3 -I - "$1" "$wrapper" <<'EOF'
import json
import sys

path, wrapper = sys.argv[1], sys.argv[2]
try:
    with open(path) as f:
        text = f.read()
    settings = json.loads(text) if text.strip() else {}
except FileNotFoundError:
    settings = {}
except ValueError:
    print(f"  {path} is not plain JSON, so it stays as it is. Add these lines yourself.")
    print(f'  "terminal.integrated.profiles.linux": {{"meersh": {{"path": "{wrapper}"}}}},')
    print('  "terminal.integrated.defaultProfile.linux": "meersh"')
    sys.exit(0)
before = json.dumps(settings, sort_keys=True)
settings.setdefault("terminal.integrated.profiles.linux", {}).setdefault("meersh", {})["path"] = wrapper
settings["terminal.integrated.defaultProfile.linux"] = "meersh"
if json.dumps(settings, sort_keys=True) == before:
    print("  unchanged")
else:
    with open(path, "w") as f:
        json.dump(settings, f, indent=4)
        f.write("\n")
    print("  meersh is the default terminal profile")
EOF
}
for dir in "$config/Code/User" "$config/Code - OSS/User" "$config/VSCodium/User"; do
    [ -d "$dir" ] || continue
    step "VS Code settings in $dir"
    if has python3; then
        vscode_settings "$dir/settings.json"
    else
        say "python3 is missing, so set terminal.integrated.defaultProfile.linux yourself"
    fi
done

step "meersh policy for agents"
mkdir -p "$config/meersh"
agents=$config/meersh/agents.msh
init=$config/meersh/init.msh
if [ -e "$agents" ]; then
    say "$agents is there already and stays as it is"
else
    {
        cat "$repo/integrations/claude/init.msh"
        printf '\n# Added by integrations/setup.sh from MEERSH_AGENT_PROGRAMS. These run arbitrary code.\n'
        printf 'policy allow agent:* exec $it.program in [%s]\n' "$programs"
    } >"$agents"
    say "wrote $agents, which allows agents to run $programs as well"
fi
if [ -f "$init" ] && grep -qF "$agents" "$init"; then
    say "init.msh loads it already"
elif [ -f "$init" ] && grep -q '^policy allow agent:' "$init"; then
    say "init.msh has its own agent policy, so it does not load $agents"
else
    {
        [ -s "$init" ] && printf '\n'
        printf '# The agent policy. It is loaded through let, because every policy command returns its\n'
        printf '# rule and the REPL would print them all at startup.\n'
        printf "let _agents = (source '%s')\n" "$agents"
    } >>"$init"
    say "init.msh loads it"
fi

if has claude; then
    step "Claude Code"
    if claude mcp get meersh >/dev/null 2>&1; then
        say "the meersh MCP server is registered already"
    else
        claude mcp add --scope user meersh -- "$wrapper" mcp --agent claude --unique >/dev/null
        say "registered the meersh MCP server for all projects"
    fi
    if has python3; then
        python3 -I - "$claude_dir/settings.json" <<'EOF'
import json
import os
import sys

path = sys.argv[1]
try:
    with open(path) as f:
        settings = json.load(f)
except FileNotFoundError:
    settings = {}
allow = settings.setdefault("permissions", {}).setdefault("allow", [])
if "mcp__meersh" in allow:
    print("  the meersh tools are in the allow list already")
else:
    allow.insert(0, "mcp__meersh")
    os.makedirs(os.path.dirname(path), exist_ok=True)
    with open(path, "w") as f:
        json.dump(settings, f, indent=2)
        f.write("\n")
    print("  added the meersh tools to the allow list")
EOF
    else
        say "python3 is missing, so add \"mcp__meersh\" to permissions.allow in $claude_dir/settings.json yourself"
    fi
    claude_md=$claude_dir/CLAUDE.md
    if [ -f "$claude_md" ] && grep -q '^## Shell work goes through meersh' "$claude_md"; then
        say "CLAUDE.md has the meersh section already"
    else
        mkdir -p "$claude_dir"
        {
            [ -s "$claude_md" ] && printf '\n'
            cat "$repo/integrations/claude/CLAUDE-meersh-fallback.md"
        } >>"$claude_md"
        say "added the meersh section to $claude_md"
    fi
    say "Claude Code sessions that are already running see meersh after a restart."
fi

if [ "$login_shell" = yes ]; then
    step "Login shell $login_path"
    if cmp -s "$repo/integrations/login-shell" "$login_path"; then
        say "installed already"
    else
        sudo install -D -m 755 "$repo/integrations/login-shell" "$login_path"
        has restorecon && sudo restorecon -R "$(dirname "$login_path")"
        say "installed"
    fi
    if grep -qx "$login_path" /etc/shells; then
        say "listed in /etc/shells already"
    else
        echo "$login_path" | sudo tee -a /etc/shells >/dev/null
        say "added to /etc/shells"
    fi
    user=$(id -un)
    if [ "$(getent passwd "$user" | cut -d: -f7)" = "$login_path" ]; then
        say "it is your login shell already"
    else
        sudo chsh -s "$login_path" "$user"
        say "it is your login shell from the next login on. sudo chsh -s /bin/bash $user undoes it."
    fi
fi

step "Done. New terminals start meersh."
