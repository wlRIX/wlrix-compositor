#!/usr/bin/env just --justfile
name := 'wlrix-compositor'

rootdir := ''
prefix := '/usr'

base-dir := absolute_path(clean(rootdir / prefix))
bin-dir := base-dir / 'bin'
# The config file goes in /etc, which is not under the prefix wherever the prefix is -- the same
# reasoning wlrix-bg's justfile gives for its own default config.
etc-dir := absolute_path(clean(rootdir / 'etc'))

bin-src := 'target' / 'release' / name
bin-dst := bin-dir / name

# The system default config. A plain file rather than a @TOKEN@ template like wlrix-bg's: what it
# sets is a cursor theme *name*, which an XCursor loader resolves by searching, so there is no
# install-time path to substitute into it.
config-src := 'data' / 'compositor.toml'
config-dst := etc-dir / 'wlrix' / 'compositor.toml'

# The cursor theme that config names. **Hand-kept in step with wlrix-assets' justfile**, which is
# the one that installs it; there is no shared variable to point at, because the repos build
# standalone. Getting it wrong is not fatal -- the compositor reports the theme it could not find
# and draws its built-in arrow -- but it is a generic pointer where an IRIX one should be.
cursor-theme := 'sgi'

default:
  @just --list

release:
  cargo build --release

lint:
  cargo clippy

test:
  cargo test

# Install the wlRIX compositor and its system default config.
#
# Deliberately does not build: this is normally run as root, and building as root leaves a
# target directory nobody can write to afterwards.
#
#     just release && sudo just install
[doc("Install the compositor and its default config (build first; run as root)")]
install:
  #!/usr/bin/env bash
  set -euo pipefail
  if [ ! -x '{{bin-src}}' ]; then
    echo "no release build -- run 'just release' first" >&2
    exit 1
  fi
  install -Dm0755 '{{bin-src}}' '{{bin-dst}}'
  echo "installed {{bin-dst}}"

  # /etc belongs to whoever installed the machine. An admin who has edited this file must not
  # have their edit thrown away by a reinstall, so an existing one is left alone and said so --
  # unlike the binary, which is ours and is replaced every time.
  if [ -e '{{config-dst}}' ]; then
    echo "kept {{config-dst}} (already present; not overwritten)"
  else
    install -Dm0644 '{{config-src}}' '{{config-dst}}'
    echo "installed {{config-dst}}"
  fi
  echo
  echo "The default config names the {{cursor-theme}} cursor theme, which wlrix-assets"
  echo "installs. Without it the pointer falls back to whatever theme this machine has."

# Remove what `install` put down.
#
# The config file is left behind on purpose: it is under /etc, it may have been edited, and an
# uninstall that silently deleted somebody's configuration would be a poor trade for a tidy
# filesystem. `wlrix-bg`'s, `wlrix-session`'s and the greeter's uninstalls leave theirs too.
[doc("Remove what install put down")]
uninstall:
  #!/usr/bin/env bash
  set -euo pipefail
  rm -f '{{bin-dst}}'
  echo "removed {{bin-dst}}"
  if [ -e '{{config-dst}}' ]; then
    echo "left {{config-dst}} alone -- remove it by hand if you want it gone"
  fi

clean:
  cargo clean
