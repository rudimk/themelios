# ThemeliOS

**ThemeliOS** (from Greek *θεμέλιο* — "foundation") is an experimental capability-based microkernel operating system written in Rust. It is designed from the ground up to do one thing well: **run container workloads securely**.

## What is ThemeliOS?

ThemeliOS is a from-scratch kernel — it does not use or build on top of Linux. It implements its own memory management, process scheduling, inter-process communication, and security model.

The long-term vision is a minimal, immutable OS that:

- Boots on virtual machines and bare metal
- Runs OCI-compatible container images
- Serves as a Kubernetes/K3s worker node
- Provides hardware-enforced isolation between containers via capabilities
- Has no SSH, no shell, and no way to "log in" — all management is via API

## Why build a new kernel?

Existing container OSes (Bottlerocket, Talos Linux, Flatcar) all use the Linux kernel with a stripped-down userspace. This is practical, but it inherits Linux's security model — namespaces and cgroups are opt-in isolation bolted onto a kernel designed for general-purpose computing.

ThemeliOS takes the opposite approach: **isolation is the default**. The capability-based security model means a process has zero access to anything unless explicitly granted. There's nothing to escape from because there's no ambient authority to escalate to.

## Project status

ThemeliOS is in early development. See the [Milestones](./milestones.md) page for the current roadmap.

## License

MIT — Copyright (c) 2026 Rudi MK
