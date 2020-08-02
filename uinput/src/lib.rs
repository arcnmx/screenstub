use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};
use std::fs::File;
use std::io;
use std::{mem, slice};
use std::task::{Poll, Context};
use std::pin::Pin;
use input_linux as input;
use input_linux::{
    UInputHandle, InputId,
    InputEvent, EventKind,
    AbsoluteAxis, RelativeAxis, Key,
    AbsoluteInfoSetup, AbsoluteInfo, Bitmask,
    EventCodec,
};
use tokio::io::{AsyncRead, AsyncWrite, PollEvented};
use tokio_util::codec::{Decoder, Encoder};
use tokio_fd::{Fd, o_nonblock};
use futures::{Sink, Stream, ready};
use bytes::BytesMut;
use log::{trace, debug};

pub type EvdevHandle<'a> = input_linux::EvdevHandle<&'a Fd<File>>;

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
        self.bits_abs.insert(setup.axis);
        self.abs.push(setup);

        self
    }

    pub fn x_config_rel(&mut self) -> &mut Self {
        self.x_config_button();
        self.bits_events.insert(EventKind::Relative);
        for &axis in &[RelativeAxis::X, RelativeAxis::Y, RelativeAxis::Wheel, RelativeAxis::HorizontalWheel] {
            self.bits_rel.insert(axis);
        }

        self
    }

    pub fn x_config_abs(&mut self) -> &mut Self {
        self.x_config_button();
        self.bits_events.insert(EventKind::Absolute);
        for &axis in &[AbsoluteAxis::X, AbsoluteAxis::Y] {
            self.absolute_axis(AbsoluteInfoSetup {
                axis,
                info: AbsoluteInfo {
                    maximum: 0x8000,
                    resolution: 1,
                    .. Default::default()
                },
            });
        }
        self.bits_events.insert(EventKind::Relative);
        for &axis in &[RelativeAxis::Wheel, RelativeAxis::HorizontalWheel] {
            self.bits_rel.insert(axis);
        }

        self
    }

    pub fn x_config_button(&mut self) -> &mut Self {
        self.bits_events.insert(EventKind::Key);
        self.bits_keys.or(Key::iter().filter(|k| k.is_button()));

        self
    }

    pub fn x_config_key(&mut self, repeat: bool) -> &mut Self {
        self.bits_events.insert(EventKind::Key);
        if repeat {
            // autorepeat is undesired, the VM will have its own implementation
            self.bits_events.insert(EventKind::Autorepeat); // kernel should handle this for us as long as it's set
        }
        self.bits_keys.or(Key::iter().filter(|k| k.is_key()));

        self
    }

    pub fn from_evdev<F: AsRawFd>(&mut self, evdev: &input_linux::EvdevHandle<F>) -> io::Result<&mut Self> {
        evdev.device_properties()?.iter().for_each(|bit| self.bits_props.insert(bit));
        evdev.event_bits()?.iter().for_each(|bit| self.bits_events.insert(bit));
        evdev.key_bits()?.iter().for_each(|bit| self.bits_keys.insert(bit));
        evdev.relative_bits()?.iter().for_each(|bit| self.bits_rel.insert(bit));
        evdev.misc_bits()?.iter().for_each(|bit| self.bits_misc.insert(bit));
        evdev.led_bits()?.iter().for_each(|bit| self.bits_led.insert(bit));
        evdev.sound_bits()?.iter().for_each(|bit| self.bits_sound.insert(bit));
        evdev.switch_bits()?.iter().for_each(|bit| self.bits_switch.insert(bit));

        // TODO: FF bits?

        for axis in &evdev.absolute_bits()? {
            // TODO: this breaks things :<
            self.absolute_axis(AbsoluteInfoSetup {
                axis,
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
        let fd = Fd::new(open.open(FILENAME)?);

        let handle = UInputHandle::new(&fd);

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
            fd,
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

    pub fn to_sink(self) -> io::Result<UInputSink> {
        let fd = Some(PollEvented::new(self.fd)?);
        //let uinput_write = FramedWrite::new(uinput_f, input::EventCodec::new());

        Ok(UInputSink {
            fd,
            buffer_write: Default::default(),
            buffer_read: Default::default(),
            codec: EventCodec::new(),
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

    pub fn to_sink(self) -> io::Result<UInputSink> {
        let fd = Some(PollEvented::new(self.fd)?);
        //let uinput_write = FramedWrite::new(uinput_f, input::EventCodec::new());

        Ok(UInputSink {
            fd,
            buffer_write: Default::default(),
            buffer_read: Default::default(),
            codec: EventCodec::new(),
        })
    }
}

//#[derive(Debug)]
pub struct UInputSink {
    fd: Option<PollEvented<Fd<File>>>,
    buffer_write: BytesMut,
    buffer_read: BytesMut,
    codec: EventCodec,
}

impl UInputSink {
    pub fn evdev(&self) -> Option<EvdevHandle> {
        self.fd.as_ref().map(|fd|
            EvdevHandle::new(fd.get_ref())
        )
    }
}

impl Sink<InputEvent> for UInputSink {
    type Error = io::Error;

    fn start_send(self: Pin<&mut Self>, item: InputEvent) -> Result<(), Self::Error> {
        trace!("UInputSink start_send({:?})", item);

        let this = unsafe { self.get_unchecked_mut() };
        this.codec.encode(item, &mut this.buffer_write)?;

        // TODO: poll_ready in here to start the write?

        Ok(())
    }

    fn poll_ready(self: Pin<&mut Self>, cx: &mut Context) -> Poll<Result<(), Self::Error>> {
        trace!("UInputSink poll_ready");

        // TODO: force full flush if buffer is over a certain amount, otherwise proceed?

        self.poll_flush(cx)
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context) -> Poll<Result<(), Self::Error>> {
        trace!("UInputSink poll_flush");

        let this = unsafe { self.get_unchecked_mut() };
        if let Some(mut fd) = Pin::new(&mut this.fd).as_pin_mut() {
            while !this.buffer_write.is_empty() {
                let n = ready!(fd.as_mut().poll_write(cx, &this.buffer_write))?;

                if n == 0 {
                    return Poll::Ready(Err(io::Error::new(io::ErrorKind::WriteZero, "failed to write to uinput")))
                }

                let _ = this.buffer_write.split_to(n);
            }

            fd.poll_flush(cx)
        } else {
            Poll::Ready(Ok(()))
        }
    }

    fn poll_close(mut self: Pin<&mut Self>, cx: &mut Context) -> Poll<Result<(), Self::Error>> {
        trace!("UInputSink poll_close");

        ready!(self.as_mut().poll_flush(cx))?;

        let res = if let Some(fd) = unsafe { self.as_mut().map_unchecked_mut(|this| &mut this.fd).as_pin_mut() } {
            fd.poll_shutdown(cx)
        } else {
            Poll::Ready(Ok(()))
        };
        if res.is_ready() {
            self.fd = None;
        }
        res
    }
}

impl Stream for UInputSink {
    type Item = Result<InputEvent, io::Error>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context) -> Poll<Option<Self::Item>> {
        trace!("UInputSink poll_next");

        let this = unsafe { self.get_unchecked_mut() };
        loop {
            if let Some(fd) = Pin::new(&mut this.fd).as_pin_mut() {
                if let Some(frame) = this.codec.decode(&mut this.buffer_read)? {
                    return Poll::Ready(Some(Ok(frame)))
                }

                this.buffer_read.reserve(mem::size_of::<InputEvent>() * 8);
                if ready!(fd.poll_read_buf(cx, &mut this.buffer_read))? == 0 {
                    this.fd = None;
                }
            } else {
                return Poll::Ready(this.codec.decode_eof(&mut this.buffer_read).transpose())
            }
        }
    }
}
