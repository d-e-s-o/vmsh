[![pipeline](https://github.com/d-e-s-o/vmsh/actions/workflows/test.yml/badge.svg?branch=main)](https://github.com/d-e-s-o/vmsh/actions/workflows/test.yml)
[![crates.io](https://img.shields.io/crates/v/vmsh.svg)](https://crates.io/crates/vmsh)
[![rustc](https://img.shields.io/badge/rustc-stable-blue.svg)](https://www.rust-lang.org)

vmsh
====

**vmsh** transparently runs a shell (or any other binary) inside a
lightweight KVM virtual machine using a provided kernel image. The host
filesystem is shared into the guest via virtiofs, and
stdin/stdout/stderr are forwarded so that everything behaves as if the
command were running locally -- except it executes in an isolated VM.
This way, as long as the current user has access to `/dev/kvm`, they can
effectively perform privileged operations without involvement of the
host kernel.

```sh
# Start an interactive shell in a VM.
$ vmsh <vmlinux>

# Run a specific command
$ vmsh <vmlinux> -- echo "hello from a VM".
> hello from a VM

# Forward all environment variables.
$ vmsh --all-envs <vmlinux> -- make -j4
```


Requirements
------------

- Rust and C toolchains
- Linux host with `/dev/kvm` access
- A Linux kernel image in vmlinux (ELF) format, optionally compressed
  with gzip, bzip2, or zstd

Minimal kernel images are available as artifacts present on [CI
runs](https://github.com/d-e-s-o/vmsh/actions/workflows/test.yml), but
you can compile a kernel from scratch as well. When building your own
kernel, you will likely want at least the following options enabled (on
top of basic ones such as `PROC_FS`, `SYSFS`, etc., of course):

- `CONFIG_HYPERVISOR_GUEST`, `CONFIG_KVM_GUEST`, `CONFIG_PARAVIRT` --
  KVM guest support
- `CONFIG_VIRTIO`, `CONFIG_VIRTIO_MMIO`,
  `CONFIG_VIRTIO_MMIO_CMDLINE_DEVICES` -- virtio transport
- `CONFIG_VIRTIO_CONSOLE` -- stdin/stdout/stderr forwarding
- `CONFIG_FUSE_FS`, `CONFIG_VIRTIO_FS` -- host filesystem sharing via
  virtiofs
- `CONFIG_SERIAL_8250`, `CONFIG_SERIAL_8250_CONSOLE` -- serial console
  output
- `CONFIG_TSI`, `CONFIG_VSOCKETS`, `CONFIG_VIRTIO_VSOCKETS` --
  networking support (note that additional [kernel
  patches](var/linux-patches/) are required for these features)

A ready-to-use minimal configuration is provided in
[`linux-config-minimal`](var/linux-config-minimal).


Building
--------

```sh
$ cargo build --release
```


GitHub Action
-------------

**vmsh** comes with a reusable GitHub Action. Add it to a workflow to
get the `vmsh` binary on `PATH` with automatic caching:

```yaml
- uses: d-e-s-o/vmsh@main
- run: vmsh <...>
```

On top of that, the
[`build-linux`](https://github.com/d-e-s-o/build-linux) GitHub Action
can be used for easily building a kernel given a configuration.
