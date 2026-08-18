#!/bin/sh
set -eu

ROOT=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
DESTDIR=${DESTDIR-}
SCOPE=
PREFIX=
MODE=
SUDO_HISTORY=
FROM_BUILD=
NON_INTERACTIVE=0
UNINSTALL=0
PURGE_CONFIG=0
PATH_UPDATE=ask
COPY=1

usage() {
  cat <<'EOF'
Usage: ./install_bedit.sh [OPTIONS]

  --scope user|system       Install for this user or all users
  --prefix PATH             Override ~/.local or /usr/local
  --mode named|transparent  Named commands only, or named plus editor shims
  --sudo-history root_and_user|root_only
                            Machine sudo-history policy (default: root_and_user)
  --from-build PATH         Directory containing all seven *-rust binaries
  --non-interactive         Use deterministic defaults and never prompt
  --path-update yes|no      Explicitly allow/decline shell PATH modification
  --uninstall               Remove only files installed by Bedit
  --purge-config            With --uninstall, also remove machine config
  --help                    Show this help

Defaults: user scope uses named mode; system scope uses transparent mode;
sudo history is mirrored to root and user.
EOF
}

die() { echo "bedit: $*" >&2; exit 2; }
need_value() { [ "$#" -ge 2 ] || die "$1 requires a value"; }

# Validate every existing component without following links. Installation roots
# are administrator-controlled; any unexpected link or non-directory fails
# closed before mkdir/install/mv can redirect a privileged write.
validate_directory_topology() {
  topology_path=$1
  case $topology_path in /*) ;; *) die "internal non-absolute install path: $topology_path" ;; esac
  topology_cursor=
  old_ifs=$IFS; IFS=/
  set -f
  # shellcheck disable=SC2086 # intentional slash splitting with globbing disabled
  set -- $topology_path
  set +f
  IFS=$old_ifs
  for topology_part in "$@"; do
    [ -n "$topology_part" ] || continue
    topology_cursor="$topology_cursor/$topology_part"
    [ ! -L "$topology_cursor" ] || die "unsafe symlink in install path: $topology_cursor"
    if [ -e "$topology_cursor" ]; then
      [ -d "$topology_cursor" ] || die "non-directory install path component: $topology_cursor"
    fi
  done
}

validate_destination() {
  destination=$1
  [ ! -L "$destination" ] || is_bedit_link "$destination" || die "unsafe destination symlink: $destination"
  if [ -e "$destination" ] && [ ! -f "$destination" ]; then
    die "unsafe non-regular install destination: $destination"
  fi
}

while [ "$#" -gt 0 ]; do
  case $1 in
    --scope) need_value "$@"; SCOPE=$2; shift 2 ;;
    --prefix) need_value "$@"; PREFIX=$2; shift 2 ;;
    --mode) need_value "$@"; MODE=$2; shift 2 ;;
    --sudo-history) need_value "$@"; SUDO_HISTORY=$2; shift 2 ;;
    --from-build) need_value "$@"; FROM_BUILD=$2; shift 2 ;;
    --non-interactive) NON_INTERACTIVE=1; shift ;;
    --path-update) need_value "$@"; PATH_UPDATE=$2; shift 2 ;;
    --uninstall) UNINSTALL=1; shift ;;
    --purge-config) PURGE_CONFIG=1; shift ;;
    --no-copy) COPY=0; shift ;; # retained for isolated legacy maintenance tests
    --help|-h) usage; exit 0 ;;
    *) die "unknown installer option: $1" ;;
  esac
done

if [ -x /usr/bin/uname ]; then
  BEDIT_UNAME=/usr/bin/uname
elif [ -x /bin/uname ]; then
  BEDIT_UNAME=/bin/uname
else
  die 'cannot determine platform securely'
fi
platform=$("$BEDIT_UNAME" -s)
case $platform in Linux|Darwin) ;; *) die 'secure installation is supported only on Linux and macOS' ;; esac

case ${SCOPE:-user} in user|system) ;; *) die '--scope must be user or system' ;; esac
case ${MODE:-named} in named|transparent) ;; *) die '--mode must be named or transparent' ;; esac
case ${SUDO_HISTORY:-root_and_user} in
  root_and_user|root_only) ;;
  root) SUDO_HISTORY=root_only ;;
  user) SUDO_HISTORY=root_and_user ;;
  *) die '--sudo-history must be root_and_user or root_only' ;;
esac
case $PATH_UPDATE in ask|yes|no) ;; *) die '--path-update must be yes or no' ;; esac
[ "$PURGE_CONFIG" -eq 0 ] || [ "$UNINSTALL" -eq 1 ] || die '--purge-config requires --uninstall'

if [ "$NON_INTERACTIVE" -eq 0 ] && [ -t 0 ]; then
  if [ -z "$SCOPE" ]; then
    printf '%s\n' 'Install Bedit for:' '  1. This user' '  2. All users'
    printf 'Selection [1]: '
    IFS= read -r answer || answer=
    case $answer in ''|1) SCOPE=user ;; 2) SCOPE=system ;; *) die 'selection must be 1 or 2' ;; esac
  fi
  if [ -z "$MODE" ]; then
    if [ "$SCOPE" = system ]; then
      default_mode=transparent; default_choice=1
      transparent_label='  1. Transparent protection of normal editor commands (default)'
      named_label='  2. Named Bedit commands only'
    else
      default_mode=named; default_choice=2
      transparent_label='  1. Transparent protection of normal editor commands'
      named_label='  2. Named Bedit commands only (default)'
    fi
    printf '%s\n' 'How should Bedit integrate with editors?' "$transparent_label" "$named_label"
    printf 'Selection [%s]: ' "$default_choice"
    IFS= read -r answer || answer=
    case $answer in '') MODE=$default_mode ;; 1) MODE=transparent ;; 2) MODE=named ;; *) die 'selection must be 1 or 2' ;; esac
  fi
  if [ -z "$SUDO_HISTORY" ]; then
    printf '%s\n' 'Where should Bedit store history for files edited via sudo?' \
      "  1. Root's and the invoking user's Bedit repositories (default)" \
      "  2. Root's Bedit repository only"
    printf 'Selection [1]: '
    IFS= read -r answer || answer=
    case $answer in ''|1) SUDO_HISTORY=root_and_user ;; 2) SUDO_HISTORY=root_only ;; *) die 'selection must be 1 or 2' ;; esac
  fi
fi

SCOPE=${SCOPE:-user}
if [ "$platform" = Darwin ] && [ "$SCOPE" != user ]; then
  die 'privileged macOS installation is unsupported; use --scope user'
fi
if [ -z "$MODE" ]; then if [ "$SCOPE" = system ]; then MODE=transparent; else MODE=named; fi; fi
if [ -z "$PREFIX" ]; then
  if [ "$SCOPE" = user ]; then
    PREFIX=${HOME:?HOME is required}/.local
  else
    PREFIX=/usr/local
  fi
fi

case $PREFIX in
  /*) ;;
  *) die '--prefix must be an absolute path' ;;
esac
case $PREFIX in /|/usr|/usr/bin) die "refusing dangerous prefix: $PREFIX" ;; esac
case $PREFIX in *"
"*|*"'"*|*'"'*|*'\'*|*'`'*|*'$'*) die 'prefix contains unsafe shell characters' ;; esac
if [ -n "$DESTDIR" ]; then case $DESTDIR in /*) ;; *) die 'DESTDIR must be absolute' ;; esac; fi
if [ "$SCOPE" = system ] && [ -z "$DESTDIR" ] && [ "$(id -u)" -ne 0 ]; then
  die 'system installation must run as root (for example, with sudo)'
fi

LIBEXEC="$DESTDIR$PREFIX/libexec/bedit"
CONFIG_DIR=${BEDIT_CONFIG_DIR:-"$DESTDIR/etc/bedit"}
CONFIG="$CONFIG_DIR/config.toml"

if [ -z "$SUDO_HISTORY" ] && [ -f "$CONFIG" ]; then
  existing_policy=$(awk -F= '/^[[:space:]]*sudo_history[[:space:]]*=/ { gsub(/[[:space:]"]/, "", $2); print $2; exit }' "$CONFIG")
  case $existing_policy in
    root|root_only) SUDO_HISTORY=root_only ;;
    user|root_and_user) SUDO_HISTORY=root_and_user ;;
  esac
fi
SUDO_HISTORY=${SUDO_HISTORY:-root_and_user}

if [ -n "${BEDIT_RUST_BIN_DIR-}" ]; then
  SOURCE=$BEDIT_RUST_BIN_DIR
elif [ -n "$FROM_BUILD" ]; then
  SOURCE=$FROM_BUILD
elif [ -d "$ROOT/payload" ]; then
  SOURCE=$ROOT/payload
else
  SOURCE=$ROOT/target/release
fi
if [ "$COPY" -eq 0 ]; then SOURCE="$LIBEXEC/current"; fi

HELPER="$SOURCE/bedit-rust"
if [ -x "$ROOT/target/release/bedit-rust" ]; then
  HELPER="$ROOT/target/release/bedit-rust"
elif [ -x "$ROOT/target/debug/bedit-rust" ]; then
  HELPER="$ROOT/target/debug/bedit-rust"
elif [ "$UNINSTALL" -eq 1 ] && [ -x "$LIBEXEC/current/bedit-rust" ]; then
  HELPER="$LIBEXEC/current/bedit-rust"
fi
[ -x "$HELPER" ] || die "trusted installer helper is unavailable: $HELPER"

# Kept in lockstep with the frozen v0.1.0 registry. The helper validates every
# component before using it and never executes an unvalidated source payload.
EDITOR_SPEC='vi:bvi,vim:bvi,view:bvi,ex:bvi,rvim:bvi,rview:bvi,nvim:bnvim,nano:bnano,rnano:bnano,pico:bpico,emacs:bemacs,emacs-nox:bemacs,xemacs:bemacs,ed:bed,micro:bvi,joe:bvi,jstar:bvi,jed:bvi,mcedit:bvi'

helper_action=install
if [ "$UNINSTALL" -eq 1 ]; then helper_action=uninstall; fi
helper_config=$CONFIG
if [ "$SCOPE" = user ] && [ -z "${BEDIT_CONFIG_DIR-}" ]; then
  helper_config=-
fi
validate_directory_topology "$DESTDIR$PREFIX"
if [ "$helper_config" != - ]; then validate_directory_topology "$CONFIG_DIR"; fi
"$HELPER" --trusted-install-helper "$helper_action" "$DESTDIR$PREFIX" "$helper_config" \
  "$MODE" "$SUDO_HISTORY" "$SOURCE" "$ROOT" "$EDITOR_SPEC" "$COPY" "$PURGE_CONFIG"

if [ "$UNINSTALL" -eq 1 ]; then
  echo "Bedit removed from $PREFIX; history was preserved."
  exit 0
fi
if [ "$SCOPE" = user ] && [ -z "${BEDIT_CONFIG_DIR-}" ]; then
  echo 'User install does not change machine sudo policy; root-and-user mirroring remains the runtime default unless an administrator sets /etc/bedit/config.toml.'
fi

if [ "$SCOPE" = user ]; then
  case :${PATH-}: in
    *:"$PREFIX/bin":*) ;;
    *)
      echo "$PREFIX/bin is not on PATH. Add: export PATH=\"$PREFIX/bin:\$PATH\""
      decision=$PATH_UPDATE
      if [ "$decision" = ask ] && [ "$NON_INTERACTIVE" -eq 0 ] && [ -t 0 ]; then
        printf 'Update your shell startup file? [y/N]: '
        IFS= read -r answer || answer=
        case $answer in y|Y|yes|YES) decision=yes ;; *) decision=no ;; esac
      fi
      if [ "$decision" = yes ]; then
        case ${SHELL-} in */zsh) startup=${HOME:?}/.zshrc ;; */bash) startup=${HOME:?}/.bashrc ;; *) startup= ;; esac
        if [ -z "$startup" ]; then
          echo 'Shell startup file is uncertain; PATH was not modified.'
        else
          line="export PATH=\"$PREFIX/bin:\$PATH\""
          if [ ! -f "$startup" ] || ! grep -Fx "$line" "$startup" >/dev/null 2>&1; then
            if [ -f "$startup" ]; then cp -p "$startup" "$startup.bedit-backup"; fi
            printf '\n%s\n' "$line" >>"$startup"
          fi
        fi
      fi
      ;;
  esac
fi

echo "Bedit installed for $SCOPE use in $PREFIX ($MODE mode)."
