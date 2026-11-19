# Midgard

Midgard runs Linux in lightweight VMs on [Managarm](https://github.com/managarm/managarm)
(and on Linux with KVM). `bifrost` runs commands, or a login shell, in those VMs.
GUI forwarding is available via Waypipe.

The Migard repository consists of:

- `bifrost`: the command line client.
- `midgard`: the daemon that runs the VMs (usually launched by systemd socket activation).
- `midgard-init`: PID 1 of the guest, which sets it up and starts `midgard-agent`.
- `midgard-agent`: the agent that runs in the guest.

Guests are x86_64 Linux or Managarm. Templates exist for: Alpine, Arch, Debian and Managarm
(nightly).

## Installing from Git

Midgard can be launched as a systemd user service, started via socket activation from `midgard.socket`.
We provide unit files for global installation in `crates/midgard/systemd`.
For development builds, you can use a drop-in service file:

```
cargo install --locked --path crates/midgard
cargo install --locked --path crates/bifrost
install -Dm644 -t ~/.config/systemd/user crates/midgard/systemd/midgard.{socket,service}
mkdir -p ~/.config/systemd/user/midgard.service.d
cat > ~/.config/systemd/user/midgard.service.d/cargo.conf <<'EOF'
[Service]
ExecStart=
ExecStart=%h/.cargo/bin/midgard
EOF
systemctl --user daemon-reload
systemctl --user enable --now midgard.socket
```