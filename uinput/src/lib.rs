#[macro_use]
extern crate log;
extern crate input_linux as input;
#[macro_use]
extern crate tokio_core;
extern crate tokio_io;
extern crate tokio_fd;
#[macro_use]
extern crate futures;
extern crate bytes;
extern crate libc;

use std::os::unix::io::{AsRawFd, FromRawFd};
use std::path::{Path, PathBuf};
use std::fs::File;
use std::io::{self, Write};
use std::{mem, slice};
use input::{
    UInputHandle, EvdevHandle, InputId,
    InputEvent, EventKind,
    AbsoluteAxis, RelativeAxis, Key,
    AbsoluteInfoSetup, AbsoluteInfo, Bitmask,
};
use input::EventCodec;
use tokio_core::reactor::{Handle, PollEvented};
use tokio_io::AsyncRead;
use tokio_io::codec::{Decoder, Encoder};
use tokio_fd::{Fd, o_nonblock};
use futures::{Sink, Stream, StartSend, Poll, Async, AsyncSink};
use bytes::BytesMut;

#[derive(Debug, Default, Clone)]
pub struct Builder {
    name: String,
    id: InputId,
    abs: Vec<AbsoluteInfoSetup>,
    bits_events: Bitmask<EventKind>,
    bits_keys: Bitmask<input::Key>,
    bits_abs: Bitmask<input::AbsoluteAxis>,
    bits_props: Bitmask<input::InputProperty>,
    bits_rel: Bitmask<input::RelativeAxis>,
    bits_misc: Bitmask<input::MiscKind>,
    bits_led: Bitmask<input::LedKind>,
    bits_sound: Bitmask<input::SoundKind>,
    bits_switch: Bitmask<input::SwitchKind>,
}

impl Builder {
    pub fn new() -> Self {
        Default::default()
    }

    pub fn name(&mut self, name: &str) -> &mut Self {
        self.name = name.to_owned();

        self
    }

    pub fn id(&mut self, id: &InputId) -> &mut Self {
        self.id = id.clone();

        self
    }

    pub fn absolute_axis(&mut self, setup: AbsoluteInfoSetup) -> &mut Self {
        self.bits_abs.set(setup.axis);
        self.abs.push(setup);

        self
    }

    pub fn x_config_rel(&mut self) -> &mut Self {
        self.x_config_();
        self.bits_events.set(EventKind::Relative);
        for &axis in &[RelativeAxis::X, RelativeAxis::Y, RelativeAxis::Wheel, RelativeAxis::HorizontalWheel] {
            self.bits_rel.set(axis);
        }

        self
    }

    pub fn x_config_abs(&mut self) -> &mut Self {
        self.x_config_();
        self.bits_events.set(EventKind::Absolute);
        for &axis in &[AbsoluteAxis::X, AbsoluteAxis::Y] {
            self.absolute_axis(AbsoluteInfoSetup {
                axis: axis,
                info: AbsoluteInfo {
                    maximum: 0x8000,
                    resolution: 1,
                    .. Default::default()
                },
            });
        }
        self.bits_events.set(EventKind::Relative);
        for &axis in &[RelativeAxis::Wheel, RelativeAxis::HorizontalWheel] {
            self.bits_rel.set(axis);
        }

        self
    }

    fn x_config_(&mut self) {
        self.bits_events.set(EventKind::Key);
        // autorepeat is undesired, the VM will have its own implementation
        //self.bits_events.set(EventKind::Autorepeat); // kernel should handle this for us as long as it's set
        self.bits_keys.or(Key::iter());
    }

    pub fn from_evdev(&mut self, evdev: &EvdevHandle) -> io::Result<&mut Self> {
        evdev.device_properties()?.iter().for_each(|bit| self.bits_props.set(bit));
        evdev.event_bits()?.iter().for_each(|bit| self.bits_events.set(bit));
        evdev.key_bits()?.iter().for_each(|bit| self.bits_keys.set(bit));
        evdev.relative_bits()?.iter().for_each(|bit| self.bits_rel.set(bit));
        evdev.misc_bits()?.iter().for_each(|bit| self.bits_misc.set(bit));
        evdev.led_bits()?.iter().for_each(|bit| self.bits_led.set(bit));
        evdev.sound_bits()?.iter().for_each(|bit| self.bits_sound.set(bit));
        evdev.switch_bits()?.iter().for_each(|bit| self.bits_switch.set(bit));

        // TODO: FF bits?

        for axis in &evdev.absolute_bits()? {
            // TODO: this breaks things :<
            self.absolute_axis(AbsoluteInfoSetup {
                axis: axis,
                info: evdev.absolute_info(axis)?,
            });
        }

        Ok(self)
    }

    pub fn create(&self) -> io::Result<UInput> {
        trace!("UInput open");
        const FILENAME: &'static str = "/dev/uinput";
        let mut open = o_nonblock();
        open.read(true);
        open.write(true);
        let f = open.open(FILENAME)?;

        let handle = UInputHandle::new(&f);

        debug!("UInput props {:?}", self.bits_props);
        for bit in &self.bits_props {
            handle.set_propbit(bit)?;
        }

        debug!("UInput events {:?}", self.bits_events);
        for bit in &self.bits_events {
            handle.set_evbit(bit)?;
        }

        debug!("UInput keys {:?}", self.bits_keys);
        for bit in &self.bits_keys {
            handle.set_keybit(bit)?;
        }

        debug!("UInput rel {:?}", self.bits_rel);
        for bit in &self.bits_rel {
            handle.set_relbit(bit)?;
        }

        debug!("UInput abs {:?}", self.bits_abs);
        for bit in &self.bits_abs {
            handle.set_absbit(bit)?;
        }

        debug!("UInput misc {:?}", self.bits_misc);
        for bit in &self.bits_misc {
            handle.set_mscbit(bit)?;
        }

        debug!("UInput led {:?}", self.bits_led);
        for bit in &self.bits_led {
            handle.set_ledbit(bit)?;
        }

        debug!("UInput sound {:?}", self.bits_sound);
        for bit in &self.bits_sound {
            handle.set_sndbit(bit)?;
        }

        debug!("UInput switch {:?}", self.bits_switch);
        for bit in &self.bits_switch {
            handle.set_swbit(bit)?;
        }

        handle.create(&self.id, self.name.as_bytes(), 0, &self.abs)?;

        Ok(UInput {
            path: handle.evdev_path()?,
            fd: Fd::new(f),
        })
    }
}

#[derive(Debug)]
pub struct UInput {
    pub fd: Fd<File>,
    pub path: PathBuf,
}

impl UInput {
    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn write_events(&mut self, events: &[InputEvent]) -> io::Result<usize> {
        UInputHandle::new(&self.fd).write(unsafe { mem::transmute(events) })
    }

    pub fn write_event(&mut self, event: &InputEvent) -> io::Result<usize> {
        let events = unsafe { slice::from_raw_parts(event as *const _, 1) };
        self.write_events(events)
    }

    pub fn to_sink(self, handle: &Handle) -> io::Result<UInputSink> {
        // we could use the same fd here but meh ownership
        let fd = unsafe {
            let fd = libc::dup(self.fd.as_raw_fd());
            if fd < 0 {
                return Err(io::Error::last_os_error())
            }
            File::from_raw_fd(fd)
        };

        let f = PollEvented::new(self.fd, &handle)?;
        //let uinput_write = FramedWrite::new(uinput_f, input::EventCodec::new());

        Ok(UInputSink {
            write: fd,
            read: f,
            buffer_write: Default::default(),
            buffer_read: Default::default(),
            eof: false,
            is_readable: false,
        })
    }
}

#[derive(Debug)]
pub struct Evdev {
    pub fd: Fd<File>,
}

impl Evdev {
    pub fn open<P: AsRef<Path>>(path: &P) -> io::Result<Self> {
        trace!("Evdev open");
        let mut open = o_nonblock();
        open.read(true);
        open.write(true);
        let f = open.open(path)?;

        Ok(Evdev {
            fd: Fd::new(f),
        })
    }

    pub fn evdev(&self) -> EvdevHandle {
        EvdevHandle::new(&self.fd)
    }

    pub fn to_sink(self, handle: &Handle) -> io::Result<UInputSink> {
        // we could use the same fd here but meh ownership
        let fd = unsafe {
            let fd = libc::dup(self.fd.as_raw_fd());
            if fd < 0 {
                return Err(io::Error::last_os_error())
            }
            File::from_raw_fd(fd)
        };

        let f = PollEvented::new(self.fd, &handle)?;
        //let uinput_write = FramedWrite::new(uinput_f, input::EventCodec::new());

        Ok(UInputSink {
            write: fd,
            read: f,
            buffer_write: Default::default(),
            buffer_read: Default::default(),
            eof: false,
            is_readable: false,
        })
    }
}

#[derive(Debug)]
pub struct UInputSink {
    read: PollEvented<Fd<File>>,
    write: File,
    buffer_write: BytesMut,
    buffer_read: BytesMut,
    eof: bool,
    is_readable: bool,
}

impl Sink for UInputSink {
    type SinkItem = InputEvent;
    type SinkError = io::Error;

    fn start_send(&mut self, item: Self::SinkItem) -> StartSend<Self::SinkItem, Self::SinkError> {
        trace!("UInputSink start_send({:?})", item);
        EventCodec::new().encode(item, &mut self.buffer_write)?;

        self.poll_complete()?;

        Ok(AsyncSink::Ready)
    }

    fn poll_complete(&mut self) -> Poll<(), Self::SinkError> {
        trace!("UInputSink poll_complete");

        while !self.buffer_write.is_empty() {
            let n = try_nb!(self.write.write(&self.buffer_write));

            if n == 0 {
                return Err(io::Error::new(io::ErrorKind::WriteZero, "failed to write to uinput"))
            }

            let _ = self.buffer_write.split_to(n);
        }

        //try_nb!(self.write.flush());

        Ok(Async::Ready(()))
    }
}

impl Stream for UInputSink {
    type Item = InputEvent;
    type Error = io::Error;

    fn poll(&mut self) -> Poll<Option<Self::Item>, Self::Error> {
        loop {
            if self.is_readable {
                let mut decoder = EventCodec::new();

                if self.eof {
                    let frame = decoder.decode_eof(&mut self.buffer_read)?;
                    return Ok(Async::Ready(frame))
                }

                if let Some(frame) = decoder.decode(&mut self.buffer_read)? {
                    return Ok(Async::Ready(Some(frame)))
                }

                self.is_readable = false;
            }

            self.buffer_read.reserve(mem::size_of::<InputEvent>() * 8);
            if try_ready!(self.read.read_buf(&mut self.buffer_read)) == 0 {
                self.eof = true
            }

            self.is_readable = true;
        }
    }
}
