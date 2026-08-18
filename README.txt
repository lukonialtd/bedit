Bedit — revision-backed editing

Bare interactive `bedit` opens the terminal UI;
the existing history, search, diff, tag, goto, restore, and listing CLI options
remain available for scripts.

NORMAL INSTALL (NO RUST OR CARGO)

Download the archive for your platform from GitHub Releases, verify it against
SHA256SUMS, unpack it, and run the included installer:

  tar -xzf bedit-linux-x86_64.tar.gz
  cd bedit-linux-x86_64
  ./install_bedit.sh

The interactive installer asks whether to install for this user or all users,
whether to use named or transparent editor commands, and how sudo edits should
select history. User-only installs default to named mode; system/all-users
installs default to transparent mode. Sudo history defaults to root-and-user.

Prebuilt archives are produced for Linux x86_64/ARM64 and macOS Intel/Apple
Silicon. GitHub release provenance can be checked with `gh attestation verify`.
Linux archives support installation and revision-producing editor workflows.
macOS archives support normal-user installation below the user's home directory
and revision-producing editor workflows using descriptor-relative repository
mutation, including history, diffs, restore/get, and TUI presentation. macOS
system installation and `root_and_user` privileged mirroring remain unsupported
and fail closed.

BUILD IT YOURSELF

  git clone <Bedit repository URL>
  cd bedit
  cargo build --release
  ./install_bedit.sh --from-build target/release

This is the same installer used by prebuilt releases. See BUILDING.md.

INSTALL LAYOUTS

User scope defaults to:

  ~/.local/bin
  ~/.local/libexec/bedit

System scope defaults to:

  /usr/local/bin
  /usr/local/libexec/bedit

System installation normally uses sudo. Bedit never installs into or overwrites
`/usr/bin`. If `~/.local/bin` is absent from PATH, the installer prints the exact
line to add and modifies `.bashrc` or `.zshrc` only after explicit approval.
Transparent interception is a PATH contract: the Bedit shim directory must
resolve before the real editor. Sudo commonly replaces PATH with `secure_path`;
for transparent `sudo vi`, `/usr/local/bin` must precede `/usr/bin` in that
policy. Bedit never edits sudoers. After installation an administrator should
compare `command -v vi` with `sudo sh -c 'command -v vi'`; both should resolve
to the intended Bedit shim. A custom sudo rule or absolute `/usr/bin/vi` bypasses
transparent interception.

EDITOR MODES

Named mode (the user-install default) installs:

  bedit  bed  bvi  bnvim  bnano  bpico  bemacs

They protect ed, Vim, Neovim, Nano, Pico, and Emacs respectively. Transparent
mode (the system-install default) installs ordinary-name shims for all supported
blocking aliases even if the underlying editor is absent. Each shim resolves the
real executable from the remaining PATH at invocation time, avoids Bedit shim
directories, and therefore protects an editor installed later without a Bedit
reinstall. A missing editor reports `bedit: <editor> is not installed`. Removing
Bedit reveals the next real editor on PATH.

Verified Linux boundaries cover vi/Vim, Neovim, Nano, Emacs, ed, and BusyBox vi.
Pico is implementation-dependent and has compatibility coverage where present.
Registry-backed blocking support is implemented for `vim`, `view`, `ex`, `rvim`,
`rview`, `rnano`, `emacs-nox`, `xemacs`, `micro`, `joe`, `jstar`, `jed`, and
`mcedit`; unavailable commands remain deterministic-test-only and
environment-unverified. GViM/MacVim, Neovide/Goneovim, and emacsclient are
classified as GUI/detaching or client/server strategies but deferred. Their
ordinary shims are not installed and protection is not claimed until native,
bounded process-ownership and completion boundaries are proven.

Release readiness distinguishes unit, component, synthetic, real-editor, and
installed-product evidence. `tests/real_editor_acceptance.sh` is the mandatory
hosted release gate; see `tests/REAL_EDITOR_ACCEPTANCE.md` for the exact matrix
and for editors that remain environment-unverified or deferred.

Useful unattended examples:

  ./install_bedit.sh --non-interactive --scope user --mode named
  sudo ./install_bedit.sh --non-interactive --scope system --mode transparent
  sudo ./install_bedit.sh --non-interactive --scope system --sudo-history root_only

SUDO HISTORY

The machine policy is `/etc/bedit/config.toml` and defaults to:

  sudo_history = "root_and_user"

The default publishes each validated sudo save first to root's central privileged
audit and then mirrors the same event into the invoking user's personal history.
`sudo_history = "root_only"` keeps only the mandatory root audit. Legacy `root`
and `user` spellings are read as `root_only` and `root_and_user` during the
pre-release transition. A user-only installation does not request privilege
merely to change `/etc`; an administrator can set the machine policy later.
There is no per-user override.

On Linux, the personal mirror is published by a dedicated child which
permanently drops supplementary groups, GID, and UID before touching the user's
repository. Existing repository components must be real directories owned by
that user; symlinks, special files, and paths outside the repository fail the
mirror without rolling back root's canonical revision. Suspicious existing
repos must be repaired manually; Bedit will not follow, replace, chown, or
auto-migrate them. Until secure parity is implemented, privileged
`root_and_user` mirroring fails closed on non-Linux systems; administrators may
select `root_only` as the immediate portable mitigation.

Revisions are allocated and fully published under a crash-released kernel lock.
Concurrent successful writers receive unique monotonic revision numbers without
silently overwriting backup, diff, access, or actor records.

Security notice: before the 2026-08-16 remediation, `root_and_user` mirror
publication could follow symlinks in an invoking user's repository while Bedit
still had root filesystem authority. Existing installations should use
`root_only` until upgraded and independently re-reviewed.

UPGRADE AND UNINSTALL

Run the same installer again to upgrade. It validates all seven binaries before
atomically switching the shared payload, backs up unrelated command files before
replacement, and cleans obsolete transparent shims when modes change.

  ./install_bedit.sh --scope user --uninstall --non-interactive
  sudo ./install_bedit.sh --scope system --uninstall --non-interactive

Uninstall removes only Bedit-owned commands, shims, manual pages, and support
payload. It restores replaced command files and preserves all Bedit history and
the machine config. `--purge-config` removes the config only when explicitly
combined with uninstall; history is still preserved.

PLATFORM STATUS

Linux editor boundaries cover Vim/vi, BusyBox vi, Neovim, Nano, Emacs, and ed or
Pico where their same-session capabilities apply. On macOS, normal-user editor
saves use descriptor-relative repository mutation and the same repository
format, including history, diffs, restore/get, and TUI presentation. Native CI
exercises real Vim, Neovim, Nano, and Emacs sessions. Normal-user installation
is supported only below the user's home directory. macOS system installation
and `root_and_user` privileged mirroring remain unsupported and fail closed.

LICENSE

Bedit is licensed under the GNU General Public License v3.0 or later
(GPL-3.0-or-later). See LICENSE for the complete licence text.
