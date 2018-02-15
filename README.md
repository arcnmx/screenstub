# screenstub

An alternative approach to a software KVM for VFIO passthrough that aims to work
much like [LookingGlass](https://github.com/gnif/LookingGlass) but without
relaying frames or incurring any sort of performance hit.

`screenstub` uses [DDC/CI](https://en.wikipedia.org/wiki/Display_Data_Channel)
to switch monitor inputs when its window is visible and/or in focus. It is
intended to be used with a window manager with virtual workspaces, and switches
to the VM input when its workspace is visible.


## Host Control

These are pretty straightforward to use when they work.

- [ddcutil](http://www.ddcutil.com/)
- [ddccontrol](https://github.com/ddccontrol/ddccontrol)


### NVIDIA

The NVIDIA proprietary drivers have had broken DDC/CI support for years now.
[There are workarounds](http://www.ddcutil.com/nvidia/) but it seems that it is
not currently possible to use DDC/CI over DisplayPort from the host.


## Guest Control

As usually a DDC/CI connection is only present on the currently active input, so
`screenstub` must issue a command to the guest operating system to instruct it
to relinquish control over the screen to the host. QEMU Guest Agent and SSH are
two common methods of executing commands inside of a guest.


### Windows

Windows applications interfacing with the screen must run as a logged in
graphical user. Services like QEMU Guest Agent or SSHd often run as a system
service and may have trouble running these commands without using other tricks.

- [ScreenBright](http://www.overclock.net/forum/44-monitors-displays/1262322-guide-display-control-via-windows-brightness-contrast-etc-ddc-ci.html) -set 0x60 3
- [ddcset](https://github.com/arcnmx/ddcset-c) 0x60 0x0f
- The NVIDIA NVAPI library exposes I2C functions and doesn't seem to require a
graphical user. If you really wanted this to work as a system service, that
could be an option.


### macOS

- [ddcctl](https://github.com/kfix/ddcctl)
- [DDC/CI Tools for OS X](https://github.com/jontaylor/DDC-CI-Tools-for-OS-X)
