# screenstub

An alternative approach to a software KVM for VFIO passthrough that aims to work
much like [LookingGlass](https://github.com/gnif/LookingGlass) but without
relaying frames.

`screenstub` uses [DDC/CI](https://en.wikipedia.org/wiki/Display_Data_Channel)
to switch monitor inputs when its window is visible and/or in focus. It is
intended to be used in fullscreen with virtual workspaces, and switches to the
VM input when its workspace is visible. Many options are available for working
with input forwarding to a VM, and if used without DDC/CI it becomes similar to
something like Synergy.

## Setup Overview

1. Install the [dependencies](#dependencies).
2. Install screenstub via [cargo](#installation).
3. Run `screenstub detect` to check that DDC/CI is working and your monitor
   is detected. You may need to enable DDC/CI in your monitor's settings, or
   [adjust settings if you have an NVIDIA card on the host](#nvidia).
4. Install and set up [qemu-ga to run on Windows startup](#qemu-guest-agent).
5. Install a [command-line DDC/CI program in Windows](#windows).
6. [Configure](#configuration) screenstub by modifying the example as necessary,
   and setting up [the QEMU sockets](#qemu-control-sockets) and [permissions](#input-permissions).

## Installation

Requires a modern stable [Rust toolchain](https://www.rust-lang.org/en-US/install.html)
to be installed, and can be installed and run like so:

    cargo install --git https://github.com/arcnmx/screenstub
    screenstub -c config.yml x

### Dependencies

- [libddcutil](http://www.ddcutil.com/) is recommended to use for optimal
performance, because exec approaches tend to take a few seconds to switch displays.
Version `0.8.6` is required but is not yet available on most distributions -
this probably will need to be compiled from source.
  - `--no-default-features` can be used to compile on systems without libddcutil
    support.
- [qemucomm](https://github.com/arcnmx/qemucomm/blob/master/qemucomm) must be
  installed, executable, and available in `$PATH` to communicate with QEMU.
- [xcb](https://xcb.freedesktop.org/)


## Configuration

An [example configuration](example-config.yml) is available to use as a starting
point. There are a few specific items that need to be set up for everything to
work. The `screenstub detect` command can be used to find information about
DDC/CI capable monitors and their inputs.

### QEMU Control Sockets

`screenstub` requires both QMP and guest agent sockets available to properly
control the VM and QEMU itself. This requires something similar command-line flags
to be passed to QEMU (note libvirt may already expose some of these for you):

    -chardev socket,path=/tmp/vfio-qga,server,nowait,id=qga0
    -device virtserialport,chardev=qga0,name=org.qemu.guest_agent.0
    -chardev socket,path=/tmp/vfio-qmp,server,nowait,id=qmp0
    -mon chardev=qmp0,id=qmp,mode=control

### Input Permissions

`screenstub` needs access to both `/dev/input/event*` devices and `/dev/uinput`.
Ensure that the user running it has proper permissions to both. udev can be used
to set up access for a particular user or group.


### Host Control

These are pretty straightforward to use when they work, however it is recommended
to use `libddcutil` directly instead.

- [ddcutil](http://www.ddcutil.com/)
- [ddccontrol](https://github.com/ddccontrol/ddccontrol)

#### NVIDIA

The NVIDIA Linux drivers have had broken DDC/CI support for years now.
[There are workarounds](http://www.ddcutil.com/nvidia/) but it seems that it is
not currently possible to use DDC/CI over DisplayPort from the host.


### Guest Control

As usually a DDC/CI connection is only present on the currently active input,
`screenstub` must issue a command to the guest operating system to instruct it
to relinquish control over the screen to the host. QEMU Guest Agent and SSH are
two common methods of executing commands inside of a guest.

#### Windows

Windows applications interfacing with the screen must run as a logged in
graphical user. Services like QEMU Guest Agent or SSHd often run as a system
service and may have trouble running these commands without adjustments.

- [ScreenBright](http://www.overclock.net/forum/44-monitors-displays/1262322-guide-display-control-via-windows-brightness-contrast-etc-ddc-ci.html) -set 0x60 3
- [ddcset](https://github.com/arcnmx/ddcset-c) 0x60 0x0f
- The NVIDIA NVAPI library exposes I2C functions and doesn't seem to require a
graphical user. If you really wanted this to work as a system service, that
could be an option.

##### QEMU Guest Agent

A recent version of qemu-ga is required to be able to execute processes in the
VM. It can be built from from source, or you may [download a compiled installer
here](https://github.com/arcnmx/aur-qemu-guest-agent-windows/releases).

It is recommended that you disable the default qemu-ga system service, and
instead schedule it to run on user login. I run the following in a batch script
on startup:

    powershell -Command "Start-Process \"C:\Program Files\Qemu-ga\qemu-ga.exe\" -WindowStyle Hidden"

This needs to run as admin, so you can use task scheduler for that pointing to
a batch file containing the above:

    schtasks /create /sc onlogon /tn qemu-ga /rl highest /tr "C:\path\to\qemu-ga.bat"

### macOS

- [ddcctl](https://github.com/kfix/ddcctl)
- [DDC/CI Tools for OS X](https://github.com/jontaylor/DDC-CI-Tools-for-OS-X)
