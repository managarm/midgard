# Midgard

Midgard runs Linux in lightweight VMs on [Managarm](https://github.com/managarm/managarm)
(and on Linux with KVM). `bifrost` runs commands, or a login shell, in those VMs.
GUI forwarding is available via Waypipe.

The Migard repository consists of:

- `bifrost`: the command line client.
- `midgard`: the daemon that runs the VMs (usually launched by systemd socket activation).
- `midgard-init`: the agent that runs in the guest.

Guests are x86_64 Linux. Templates exist for: Alpine, Arch and Debian.
