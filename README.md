# screenstub

An alternative approach to a software KVM switch for GPU passthrough that aims
to appear much like [LookingGlass](https://github.com/gnif/LookingGlass) but
without relaying frames.

`screenstub` uses [DDC/CI](https://en.wikipedia.org/wiki/Display_Data_Channel)
to switch monitor inputs when its window is visible and/or in focus. It is
intended to be used in fullscreen with virtual workspaces, and switches to the
VM input when its workspace is visible. Many options are available for working
with input forwarding to a VM, and if used without DDC/CI it becomes similar to
something like Synergy.

## Setup Overview

1. Install screenstub via [cargo](#installation).
2. Run `screenstub detect` to check that DDC/CI is working and your monitor
   is detected. You may need to enable DDC/CI in your monitor's settings, and/or
   [load the i2c drivers](#host-control).
3. [Configure](#configuration) screenstub by modifying the example as necessary,
   and setting up [the QEMU sockets](#qemu-control-sockets).

Additional configuration may be required for advanced usage:

1. Install and set up [qemu-ga to run on Windows startup](#qemu-guest-agent).
2. Install a [command-line DDC/CI program in Windows](#windows).
3. Configure [permissions](#uinput-permissions), and check your [input devices](#guest-input-devices) in order to use the advanced routing modes.

## Installation

Requires a modern stable [Rust toolchain](https://www.rust-lang.org/en-US/install.html),
and can be installed and run like so:

    cargo install --force --git https://github.com/arcnmx/screenstub
    screenstub -c config.yml x

### Dependencies

- udev (Debian: libudev-dev)

### Packages

- [Nix{,OS}](https://github.com/arcnmx/nixexprs): `nix run -f https://github.com/nix-community/NUR/archive/master.tar.gz repos.arc.packages.screenstub -c screenstub`

## Configuration

An [example configuration](samples/config.yml) is available to use as a starting
point. There are a few specific items that need to be set up for everything to
work. The `screenstub detect` command can be used to find information about
DDC/CI capable monitors and their inputs.

### QEMU Control Sockets

`screenstub` requires both QMP and guest agent sockets available to properly
control the VM and QEMU itself. This requires something similar to the following
command-line flags to be passed to QEMU (note libvirt may already expose some of
these for you):

    -chardev socket,path=/tmp/vfio-qga,server,nowait,id=qga0
    -device virtserialport,chardev=qga0,name=org.qemu.guest_agent.0
    -chardev socket,path=/tmp/vfio-qmp,server,nowait,id=qmp0
    -mon chardev=qmp0,id=qmp,mode=control

### Guest Input Devices

`screenstub` emulates three different input devices: a keyboard, a tablet for
absolute coordinate mouse mapping, and a mouse for relative motion events. Each
of these devices may be configured as USB devices, PS/2, or virtio. It is
recommended that you remove all existing input devices from your QEMU command
line configuration (`-usbdevice kbd`, `-usbdevice mouse`, `-usbdevice tablet`, `-device usb-kbd`, `-device usb-mouse`, `-device usb-tablet`).

The default configuration sets them up for optimal compatibility, but virtio
input drivers (vioinput) are recommended instead for performance reasons. These
require drivers to be installed in the guest. You can [download them for Windows here](https://docs.fedoraproject.org/en-US/quick-docs/creating-windows-virtual-machines-using-virtio-drivers/index.html).

### Input Event Routing

The routing mode describes how input events are translated from the host mouse
and keyboard to the guest devices. The default `qmp` routing mode sends all input
commands over the QEMU control socket. This requires no additional configuration
on the host, but may not be optimal for performance. The other routing modes use
`uinput` instead to transport events, which requires additional configuration.

#### UInput Permissions

To use the `virtio-host` or `input-linux` routing modes, `screenstub` needs
access to `/dev/uinput` and the virtual `/dev/input/event*` devices.
[udev rules](samples/udev/rules.d/99-uinput.rules) can be used to set up device
permissions. Additional rules may be included for any external devices you want
to "grab" and forward to the guest.

Xorg also needs to be configured to ignore the virtual devices. Copy
[the xorg config](samples/xorg.conf.d/30-screenstub.conf) into your `xorg.conf` or
`/etc/X11/xorg.conf.d/` directory to prevent Xorg from trying to use the virtual
input devices for the host.

### Host Control

These are pretty straightforward to use when they work, however it is recommended
to use the built-in DDC/CI support instead. You will probably need to load the `i2c-dev`
kernel module for it to work, by placing [i2c.conf](samples/modules-load.d/i2c.conf)
in `/etc/modules-load.d/`.

- [ddcutil](http://www.ddcutil.com/)
- [ddccontrol](https://github.com/ddccontrol/ddccontrol)

#### NVIDIA

Some NVIDIA Linux drivers have had broken DDC/CI support.
[There are workarounds](http://www.ddcutil.com/nvidia/) but there may be issues
when using DDC/CI over DisplayPort from the host.

### Guest Control

Many monitors require a DDC/CI command to be issued by the GPU on the currently
active input. In this case `screenstub` must issue a command to the guest OS
to instruct it to relinquish control over the screen back to the host.
QEMU Guest Agent and SSH are two common methods of executing commands inside
of a guest.

#### Windows

- [ddcset](https://github.com/arcnmx/ddcset-rs) setvcp 60 3
- [ScreenBright](http://www.overclock.net/forum/44-monitors-displays/1262322-guide-display-control-via-windows-brightness-contrast-etc-ddc-ci.html) -set 0x60 3
- [ClickMonitorDDC](https://clickmonitorddc.bplaced.net/) s DVI1

Note that Windows applications interfacing with the screen must run as a logged
in graphical user. Services like QEMU Guest Agent or SSHd often run as a system
service and may have trouble running these commands without adjustments. NVAPI
for example (via `ddcset -b nvapi`) may be used as an alternative on NVIDIA cards.

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
