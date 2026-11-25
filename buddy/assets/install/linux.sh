#!/bin/sh
# Invoked by the verified install command from an existing Buddy device.
set -eu
umask 077
url=$1
sha=$2
size=$3
ca=$4
[ "$(uname -sm)" = "Linux x86_64" ] || { echo 'This build requires x86-64 Linux.' >&2; exit 1; }
in_path() {
    case ":$PATH:" in *":$1:"*) return 0 ;; *) return 1 ;; esac
}
dir=${XDG_BIN_HOME:-"$HOME/.local/bin"}
if [ -z "${XDG_BIN_HOME:-}" ] && ! in_path "$dir" && [ -d "$HOME/bin" ] && in_path "$HOME/bin"; then
    dir=$HOME/bin
fi
mkdir -p -- "$dir"
target=$dir/buddy
staged=$(mktemp "$dir/.buddy-install.XXXXXXXX") || exit 1
trap 'rm -f -- "$staged"' EXIT
if [ -n "$ca" ]; then set -- --cacert "$ca"; else set --; fi
curl --fail --silent --show-error --proto '=http,https' --max-filesize "$size" "$@" --output "$staged" -- "$url"
[ "$(wc -c < "$staged" | tr -d ' ')" = "$size" ] || { echo 'Incomplete Buddy download.' >&2; exit 1; }
actual=$(sha256sum < "$staged")
[ "${actual%% *}" = "$sha" ] || { echo 'Buddy download failed verification.' >&2; exit 1; }
chmod 755 "$staged"
same_install() {
    [ -f "$target" ] && [ ! -L "$target" ] && [ -x "$target" ] && cmp -s "$staged" "$target"
}
install_conflict() {
    echo "Already installed at $target. Update Buddy in Settings, or move the existing executable aside and rerun this command." >&2
    exit 1
}
candidate=$staged
if [ -e "$target" ] || [ -L "$target" ]; then
    same_install || install_conflict
    candidate=$target
fi
set --
if [ "${BUDDY_ALLOW_FILE_CREDENTIAL:-0}" = 1 ]; then set -- --private-file-credential; fi
"$candidate" redeem-device "$@"
# Publish a fresh install only after enrollment. The link leaves any existing executable alone.
if [ "$candidate" = "$staged" ] && ! ln -T -- "$staged" "$target" 2>/dev/null; then
    same_install || install_conflict
fi
printf 'Installed %s\n' "$target"
if ! in_path "$dir"; then
    if [ -n "${XDG_BIN_HOME:-}" ]; then path_entry='$XDG_BIN_HOME'; else path_entry='$HOME/.local/bin'; fi
    case "${SHELL:-}" in
        */fish) printf 'To use the buddy command, run:\n  fish_add_path "%s"\n' "$path_entry" ;;
        *) printf 'To use the buddy command in this shell, run:\n  export PATH="%s:$PATH"\n' "$path_entry" ;;
    esac
fi
finish() {
    if [ -t 1 ] && [ -r /dev/tty ]; then "$target" finish-install </dev/tty; else "$target" finish-install </dev/null; fi
}
finish || printf '%s\n' "Buddy is installed and paired. Open Buddy to finish Steam setup later." >&2
