extern crate void;
extern crate xcb;
extern crate mio;
#[macro_use]
extern crate futures;
extern crate futures_cpupool;
extern crate input_linux as input;
extern crate tokio_file_unix;
extern crate tokio_process;
extern crate tokio_core;
extern crate tokio_io;
#[macro_use]
extern crate quick_error;
extern crate ddcutil;

mod error;
mod fd;
use fd::Fd;
mod send_async;
use send_async::StreamUnzipExt;
mod ddc;

use error::Error;
use std::thread::spawn;
use std::process::exit;
use std::{fs, vec, iter};
use std::sync::{Arc, Mutex};
use std::collections::HashMap;
use std::rc::Rc;
use std::cell::RefCell;
use std::io::{self, Write};
use std::os::unix::io::AsRawFd;
use std::process::Command;
use tokio_core::reactor::{Core, Handle, PollEvented};
use tokio_file_unix::File as TokioFile;
use tokio_io::codec::{FramedRead, FramedWrite};
use tokio_process::CommandExt;
use input::{InputEvent, EventRef, EventKind, EvdevHandle, UInputHandle, Bitmask, Key};
use futures::{future, Future, Stream, Sink};
use futures::stream::{self, iter_ok};
use futures::sync::mpsc;
use futures::unsync::mpsc as un_mpsc;
use futures_cpupool::CpuPool;

fn main() {
	match main_result() {
		Ok(res) => exit(res),
		Err(err) => {
			panic!("boom {:?}", err)
		},
	}
}

fn handle_visible_event(visible: bool) -> io::Result<()> {
	use std::process::Command;

	let res = Command::new("vm")
		.arg("windows")
		.arg(if visible { "video_input_guest" } else { "video_input_host" })
		.spawn().map(drop);

	if let Err(ref err) = res {
		println!("Failed to spawn event handler: {:?}", err);
	}

	res
}

#[derive(Debug)]
enum XEvent {
	Visible(bool),
	Leave,
	Resize {
		width: u16,
		height: u16,
	},
	Mouse {
		x: i16,
		y: i16,
	},
	Button {
		pressed: bool,
		detail: xcb::Button,
		state: u16,
	},
	Key {
		pressed: bool,
		keycode: xcb::Keycode,
		keysym: Option<xcb::Keysym>,
		state: u16,
	},
}

#[derive(Debug)]
enum TerminalEvent {
	Error(Error),
	Exit(i32),
}

#[derive(Debug)]
enum UserEvent {
	ShowHost,
	ShowGuest,
	Exec(Vec<String>),
	ToggleGrab,
	Grab,
	Ungrab,
}

fn send_event<T, I: Into<T>>(handle: &mut mpsc::Sender<T>, event: I) -> Result<(), Error> {
	match handle.try_send(event.into()) {
		Ok(_) => Ok(()),
		Err(ref err) if err.is_full() => {
			unimplemented!()
		},
		Err(ref err) if err.is_disconnected() => Err(Error::Exit),
		_ => unreachable!(),
	}
}

#[derive(Debug)]
struct Hotkey {
	triggers: Vec<Key>,
	modifiers: Vec<Key>,
	event: Rc<UserEvent>,
}

enum XEventProcessed {
	User(UserEvent),
	Input(InputEvent),
}

impl From<UserEvent> for XEventProcessed {
	fn from(e: UserEvent) -> Self {
		XEventProcessed::User(e)
	}
}

impl From<InputEvent> for XEventProcessed {
	fn from(e: InputEvent) -> Self {
		XEventProcessed::Input(e)
	}
}

#[derive(Default)]
struct XEventHandler {
	width: u16,
	height: u16,
	prev_x: i16,
	prev_y: i16,
	evdev: Vec<EvdevHandle>,
	grabbing: bool,
	keys: Bitmask<Key>,
	triggers_press: HashMap<Key, Vec<Rc<Hotkey>>>,
	triggers_release: HashMap<Key, Vec<Rc<Hotkey>>>,
	ddc_input: Arc<ddc::SearchInput>,
	ddc_monitor: Arc<Mutex<ddc::Monitor>>,
}

impl XEventHandler {
	pub fn add_hotkey(&mut self, hotkey: Hotkey, press: bool) {
		let hotkey = Rc::new(hotkey);
		for trigger in &hotkey.triggers {
			if press {
				&mut self.triggers_press
			} else {
				&mut self.triggers_release
			}.entry(*trigger).or_insert(Vec::new()).push(hotkey.clone());
		}
	}

	fn event_time(&self) -> input::EventTime {
		unsafe { ::std::mem::zeroed() }
	}

	fn key_state(pressed: bool) -> i32 {
		match pressed {
			true => input::KeyState::Pressed.into(),
			false => input::KeyState::Released.into(),
		}
	}

	fn map_button(&self, button: xcb::Button) -> Option<Key> {
		match button as _ {
			/*xcb::BUTTON_INDEX_1 => Some(Key::ButtonLeft),
			xcb::BUTTON_INDEX_2 => Some(Key::ButtonMiddle),
			xcb::BUTTON_INDEX_3 => Some(Key::ButtonRight),
			xcb::BUTTON_INDEX_4 => Some(Key::Button?),
			xcb::BUTTON_INDEX_5 => Some(Key::Button?),*/
			xcb::BUTTON_INDEX_1 => Some(Key::Button0),
			xcb::BUTTON_INDEX_2 => Some(Key::Button1),
			xcb::BUTTON_INDEX_3 => Some(Key::Button2),
			xcb::BUTTON_INDEX_4 => Some(Key::Button3),
			xcb::BUTTON_INDEX_5 => Some(Key::Button4),
			unk => None,
		}
	}

	fn map_keycode(&self, key: xcb::Keycode) -> Option<Key> {
		match Key::from_code(key as _) {
			Ok(code) => Some(code),
			Err(..) => {
				println!("unknown keycode {}", key);
				None
			},
		}
	}

	fn map_keysym(&self, key: xcb::Keysym) -> Option<Key> {
		unimplemented!()
	}

	fn sync(&self, events: &mut Vec<XEventProcessed>) {
		if !events.is_empty() {
			events.push(XEventProcessed::Input(
				input::SynchronizeEvent::new(
					self.event_time(),
					input::SynchronizeKind::Report,
					0
				).into()
			));
		}
	}

	fn trigger_leave(&mut self, leave_sender: un_mpsc::Sender<InputEvent>) -> Box<Future<Item=(), Error=Error>> {
		let events: Vec<_> = self.leave_events().collect();
		self.keys = Default::default();
		Box::new(
			stream::iter_ok(events).fold(leave_sender, |sender, item|
				sender.send(item).map_err(Error::from)
			).map(drop)
		) as Box<_>
	}

	fn ddc_host(&self) -> Box<Future<Item=(), Error=Error>> {
		println!("TODO: ddc_host");
		Box::new(future::ok(())) as Box<_>
		//unimplemented!()
	}

	fn ddc_guest(&self, ddc_remote: &CpuPool) -> Box<Future<Item=(), Error=Error>> {
		let monitor = self.ddc_monitor.clone();
		let input = self.ddc_input.clone();
		Box::new(futures::sync::oneshot::spawn_fn(move || {
			let mut monitor = monitor.lock()?;
			monitor.to_display()?;
			if let Some(input) = monitor.match_input(&input) {
				monitor.set_input(input)
			} else {
				Err(Error::DdcNotFound)
			}
		}, ddc_remote)) as Box<_>
	}

	fn handle_user_event(&mut self, event: &UserEvent, leave_sender: un_mpsc::Sender<InputEvent>, handle: &Handle, ddc_remote: &CpuPool) -> Result<Box<Future<Item=(), Error=Error>>, Error> {
		println!("user event {:?}", event);

		Ok(match *event {
			UserEvent::ShowHost => {
				Box::new(stream::futures_unordered(vec![
					self.ddc_host(),
					self.trigger_leave(leave_sender),
				]).for_each(|_| Ok(()))) as Box<_>
			},
			UserEvent::ShowGuest => {
				self.ddc_guest(ddc_remote)
			},
			UserEvent::Exec(ref args) => {
				let (cmd, args) = args.split_at(1);

				let child = Command::new(&cmd[0]).args(args).spawn_async(handle);
				Box::new(future::result(child).and_then(|c| c).map_err(Error::from).and_then(Error::from_exit_status)) as Box<_>
			},
			UserEvent::ToggleGrab => {
				let ev = if self.grabbing {
					UserEvent::Ungrab
				} else {
					UserEvent::Grab
				};
				return self.handle_user_event(&ev, leave_sender, handle, ddc_remote)
			},
			UserEvent::Grab => {
				for evdev in &self.evdev {
					evdev.grab(true)?;
				}
				self.grabbing = true;
				self.trigger_leave(leave_sender)
			},
			UserEvent::Ungrab => {
				for evdev in &self.evdev {
					evdev.grab(false)?;
				}
				self.grabbing = false;
				self.trigger_leave(leave_sender)
			},
		})
	}

	fn handle_input_event(&mut self, e: EventRef) -> stream::IterOk<vec::IntoIter<Rc<UserEvent>>, Error> {
		//println!("event {:?}", e);

		iter_ok(match e {
			EventRef::Key(key) => match key.key_state() {
				input::KeyState::Released => {
					self.keys.clear(key.key);

					Default::default()
				},
				input::KeyState::Pressed => {
					self.keys.set(key.key);

					let mut events = Vec::new();
					if let Some(hotkeys) = self.triggers_press.get(&key.key) {
						for hotkey in hotkeys {
							if hotkey.triggers.iter().chain(hotkey.modifiers.iter()).all(|k| self.keys.get(*k)) {
								events.push(hotkey.event.clone());
							}
						}
					}

					if !events.is_empty() {
						// TODO: trigger leave event to remove all holds (make this a user event?)
					}

					events
				},
				_ => Default::default(),
			},
			_ => Default::default(),
		})
	}

	fn filter_evdev(&self, e: &InputEvent) -> bool {
		return self.grabbing;
	}

	fn key_event(&mut self, key: Key, pressed: bool) -> Vec<XEventProcessed> {
		vec![
			XEventProcessed::Input(
				input::KeyEvent::new(
					self.event_time(),
					key,
					Self::key_state(pressed)
				).into()
			),
			XEventProcessed::Input(
				input::SynchronizeEvent::new(
					self.event_time(),
					input::SynchronizeKind::Report,
					0
				).into()
			),
		]
	}

	// -> impl Iterator would be amazing right about now
	fn leave_events(&mut self) -> iter::Chain<iter::Map<input::bitmask::BitmaskIterator<Key>, fn(Key) -> InputEvent>, iter::Once<InputEvent>> {
		fn key_event(key: Key) -> InputEvent {
			input::KeyEvent::new(Default::default(), key, XEventHandler::key_state(false)).into()
		}

		self.keys.iter().map(key_event as _)
			.chain(iter::once(input::SynchronizeEvent::new(
				self.event_time(),
				input::SynchronizeKind::Report,
				0
			).into()))
	}

	// TODO: replace with a smallvec
	fn handle_xevent(&mut self, e: XEvent) -> stream::IterOk<vec::IntoIter<XEventProcessed>, Error> {
		iter_ok(match e {
			XEvent::Visible(visible) => {
				vec![
					XEventProcessed::User(
						if visible { UserEvent::ShowGuest } else { UserEvent::ShowHost }
					),
				]
			},
			XEvent::Leave => {
				let events = self.leave_events().map(XEventProcessed::Input).collect();
				self.keys = Default::default();
				events
			},
			XEvent::Mouse { x, y } => {
				let mut events = Vec::new();
				if x != self.prev_x && self.width > 0 {
					self.prev_x = x;
					events.push(XEventProcessed::Input(
						input::AbsoluteEvent::new(
							self.event_time(),
							input::AbsoluteAxis::X,
							(x as isize * 0x2000 / self.width as isize) as i32
						).into()
					));
				}
				if y != self.prev_y  && self.height > 0 {
					self.prev_y = y;
					events.push(XEventProcessed::Input(
						input::AbsoluteEvent::new(
							self.event_time(),
							input::AbsoluteAxis::Y,
							(y as isize * 0x2000 / self.height as isize) as i32
						).into()
					));
				}
				self.sync(&mut events);

				events
			},
			XEvent::Resize { width, height } => {
				self.width = width;
				self.height = height;

				Default::default()
			},
			XEvent::Button { pressed, detail, state } => {
				if let Some(button) = self.map_button(detail) {
					self.key_event(button, pressed)
				} else {
					println!("unknown button {}", detail);
					Default::default()
				}
			},
			XEvent::Key { pressed, keycode, keysym, state } => {
				if let Some(key) = self.map_keycode(keycode) {
					self.key_event(key, pressed)
				} else {
					println!("unknown keycode {} keysym {:?}", keycode, keysym);
					Default::default()
				}
			},
		})
	}
}

fn evdev_read(filename: &str, handle: &Handle) -> Result<(EvdevHandle, FramedRead<PollEvented<Fd<io::BufReader<fs::File>>>, input::EventCodec>), Error> {
	/*let f = fs::File::open(FILENAME)?;
	let f = TokioFile::new_nb(f)?;
	let f = PollEvented::new(f, &handle).unwrap();*/
	let f = fd::open_options();
	let f = f.open(filename)?;
	let evdev = EvdevHandle::new(&f);
	let f = PollEvented::new(Fd::from_fd(f.as_raw_fd(), io::BufReader::with_capacity(24*0x10, f)), &handle)?;
	let read = FramedRead::new(f, input::EventCodec::new());

	Ok((evdev, read))
	/*handle.spawn(read.for_each(|e| {
		Ok(println!("event {:?}", InputEvent::from_raw(&e)))
	}).map_err(drop));*/
}

fn uinput_create() -> Result<(UInputHandle, fs::File), Error> {
	const FILENAME: &'static str = "/dev/uinput";
	let mut f = fd::open_options();
	f.write(true);
	let f = f.open(FILENAME)?;
	let uinput = UInputHandle::new(&f);

	Ok((uinput, f))
}

fn main_result() -> Result<i32, Error> {
	let (uinput, _uinput) = uinput_create()?;
	let uinput = Arc::new(uinput);

	let xevents = Rc::new(RefCell::new(XEventHandler::default()));
	xevents.borrow_mut().ddc_input = Arc::new(ddc::SearchInput {
		name: Some("DisplayPort-1".into()),
		.. Default::default()
	});
	xevents.borrow_mut().ddc_monitor = Arc::new(Mutex::new(ddc::Monitor::new(
		ddc::Search {
			manufacturer_id: Some("GSM".into()),
			model_name: Some("LG Ultra HD".into()),
			.. Default::default()
		}
	)));

	xevents.borrow_mut().add_hotkey(Hotkey {
		triggers: vec![Key::KeyG],
		modifiers: vec![Key::KeyLeftMeta],
		event: Rc::new(UserEvent::ToggleGrab),
	}, true);

	let mut core = Core::new().unwrap();
	let handle = core.handle();
	let remote = core.remote();

	let mut bits_events = Bitmask::<EventKind>::default();
	let mut bits_keys = Bitmask::<Key>::default();
	let mut bits_abs = Bitmask::<input::AbsoluteAxis>::default();
	let mut props = Bitmask::<input::InputProperty>::default();
	let mut bits_rel = Bitmask::<input::RelativeAxis>::default();
	let mut bits_misc = Bitmask::<input::MiscKind>::default();
	let mut bits_led = Bitmask::<input::LedKind>::default();
	let mut bits_sound = Bitmask::<input::SoundKind>::default();
	let mut bits_switch = Bitmask::<input::SwitchKind>::default();

	// X window events
	bits_events.set(EventKind::Key);
	bits_events.set(EventKind::Autorepeat); // kernel should handle this for us I think?
	bits_events.set(EventKind::Absolute);
	//bits_keys.or(Key::iter());
	bits_abs.set(input::AbsoluteAxis::X);
	bits_abs.set(input::AbsoluteAxis::Y);

	let uinput_id = input::InputId {
		bustype: input::sys::BUS_VIRTUAL,
		vendor: 0x16c0,
		product: 0x05df,
		version: 1,
	};

	let uinput_abs = [
		input::AbsoluteInfoSetup {
			axis: input::AbsoluteAxis::X,
			info: input::AbsoluteInfo {
				value: 0,
				minimum: 0,
				maximum: 0x2000,
				fuzz: 0,
				flat: 0,
				resolution: 1,
			},
		},
		input::AbsoluteInfoSetup {
			axis: input::AbsoluteAxis::Y,
			info: input::AbsoluteInfo {
				value: 0,
				minimum: 0,
				maximum: 0x2000,
				fuzz: 0,
				flat: 0,
				resolution: 1,
			},
		},
	];

	let (mut send_event, recv_event) = un_mpsc::channel::<InputEvent>(0x10);
	let (mut send_user, recv_user) = un_mpsc::channel::<Rc<UserEvent>>(0x10);
	let (mut send_term, recv_term) = mpsc::channel::<TerminalEvent>(0x10);

	let mut evdev_handlers = Vec::new();

	let evdevs = ["/dev/input/by-id/usb-Razer_Razer_Naga_2014-if02-event-kbd", "/dev/input/by-id/usb-Razer_Razer_Naga_2014-event-mouse"];
	for filename in &evdevs {
		let (evdev, read) = evdev_read(filename, &handle)?;

		let id = evdev.device_id()?;
		let name = String::from_utf8(evdev.device_name()?);
		println!("opened evdev {} = {:?}", name.unwrap_or("(null)".into()), id);

		for prop in &evdev.device_properties()? {
			props.set(prop);
		}

		for event in &evdev.event_bits()? {
			bits_events.set(event);
		}

		for bit in &evdev.key_bits()? {
			bits_keys.set(bit);
		}

		for bit in &evdev.relative_bits()? {
			bits_rel.set(bit);
		}

		for bit in &evdev.absolute_bits()? {
			bits_abs.set(bit);
		}

		for bit in &evdev.misc_bits()? {
			bits_misc.set(bit);
		}

		for bit in &evdev.led_bits()? {
			bits_led.set(bit);
		}

		for bit in &evdev.sound_bits()? {
			bits_sound.set(bit);
		}

		for bit in &evdev.switch_bits()? {
			bits_switch.set(bit);
		}

		xevents.borrow_mut().evdev.push(evdev);

		// ff bits?

		let send_event = send_event.clone();
		let send_term = send_term.clone();
		let uinput = uinput.clone();
		let xevents = xevents.clone();
		evdev_handlers.push(
			read.map_err(Error::from)
			.filter(move |e| xevents.borrow_mut().filter_evdev(e))
			.forward(send_event).map(drop).map_err(Error::from)
			.map_err(TerminalEvent::Error).or_else(|e| send_term.send(e).map(drop)).map_err(drop)
		);
	}

	println!("props {:?}", props);
	for bit in &props {
		uinput.set_propbit(bit)?;
	}

	println!("events {:?}", bits_events);
	for bit in &bits_events {
		uinput.set_evbit(bit)?;
	}

	println!("keys {:?}", bits_keys);
	for bit in &bits_keys {
		uinput.set_keybit(bit)?;
	}

	for bit in &bits_rel {
		uinput.set_relbit(bit)?;
	}

	for bit in &bits_abs {
		uinput.set_absbit(bit)?;
	}

	for bit in &bits_misc {
		uinput.set_mscbit(bit)?;
	}

	for bit in &bits_led {
		uinput.set_ledbit(bit)?;
	}

	for bit in &bits_sound {
		uinput.set_sndbit(bit)?;
	}

	for bit in &bits_switch {
		uinput.set_swbit(bit)?;
	}

	//evdev.grab(true)?;

	uinput.create(&uinput_id, b"screenstub", 0, &uinput_abs)?;
	println!("uinput: {}", uinput.evdev_path()?.display());

	let (mut send_x, recv_x) = mpsc::channel::<XEvent>(0x10);

	{
		let send_term = send_term.clone();
		let _join = spawn(move || {
			use std::panic::{catch_unwind, AssertUnwindSafe};

			let res = {
				let send_term = send_term.clone();
				catch_unwind(AssertUnwindSafe(|| match xmain(&mut send_x) {
					Ok(res) => remote.spawn(move |h| send_term.send(TerminalEvent::Exit(res)).map(drop).map_err(drop)),
					Err(err) => remote.spawn(move |h| send_term.send(TerminalEvent::Error(err)).map(drop).map_err(drop)),
				}))
			};

			if let Err(err) = res {
				let err = if let Some(err) = err.downcast_ref::<&str>() {
					(*err).into()
				} else if let Some(err) = err.downcast_ref::<String>() {
					err
				} else {
					"X thread panic"
				};

				let err = io::Error::new(io::ErrorKind::Other, err).into();
				remote.spawn(move |h| send_term.send(TerminalEvent::Error(err)).map(drop).map_err(drop))
			}
		});
	}

	{
		let mut xevents = xevents.clone();
		let send_user = send_user.clone();
		let send_term = send_term.clone();
		let send_term2 = send_term.clone();
		let send_event = send_event.clone();
		handle.spawn(recv_x.map_err(|_| -> Error { unreachable!() })
			.map(move |e| xevents.borrow_mut().handle_xevent(e)).flatten()
			/*.and_then(move |e| match e {
				XEventProcessed::User(e) => future::Either::A(send_user.clone().send(Rc::new(e)).map_err(Error::from).map(drop)),
				XEventProcessed::Input(e) => future::Either::B(send_event.clone().send(e).map_err(Error::from).map(drop)),
			}).for_each(|_| Ok(()))*/
			.map(move |e| match e {
				XEventProcessed::User(e) => (Some(Rc::new(e)), None),
				XEventProcessed::Input(e) => (None, Some(e)),
			}).unzip_spawn(&handle, |s|
				s.filter_map(|e| e).map_err(|_| -> Error { unreachable!() }).forward(send_event).map_err(Error::from).map(drop)
				.map_err(TerminalEvent::Error).or_else(|e| send_term2.send(e).map(drop)).map_err(drop)
			).filter_map(|e| e).forward(send_user).map(drop)
			//.forward(send_event).map(drop).map_err(Error::from)
			.map_err(TerminalEvent::Error).or_else(|e| send_term.send(e).map(drop)).map_err(drop)
		);
	}

	for handler in evdev_handlers {
		handle.spawn(handler);
	}

	let uinput_f = PollEvented::new(Fd::new(_uinput), &handle)?;
	let uinput_write = FramedWrite::new(uinput_f, input::EventCodec::new());
	{
		let xevents = xevents.clone();
		let send_term = send_term.clone();
		handle.spawn(recv_event.map_err(|_| -> Error { unreachable!() })
			.and_then(move |e| {
				use std::slice;
				let slice = unsafe { slice::from_raw_parts(e.as_raw() as *const _, 1) };
				uinput.write(slice).map(|_| e).map_err(From::from)
			})
			.map(move |e| {
				if let Ok(e) = EventRef::new(&e) {
					xevents.borrow_mut().handle_input_event(e)
				} else {
					panic!("bad event {:?}", e)
				}
			}).flatten()
			.forward(send_user).map(drop).map_err(Error::from)
			//}).forward(uinput_write).map(drop).map_err(Error::from)
			.map_err(TerminalEvent::Error).or_else(|e| send_term.send(e).map(drop)).map_err(drop)
		);
	}

	let pool = {
		use futures_cpupool::Builder;
		let mut builder = Builder::new();
		builder.pool_size(1);
		builder.name_prefix("ddc");
		builder.create()
	};

	{
		let xevents = xevents.clone();
		let pool = pool.clone();
		let handle2 = handle.clone();
		handle.spawn(recv_user.map_err(|_| -> Error { unreachable!() })
			.map(move |e| xevents.borrow_mut().handle_user_event(&e, send_event.clone(), &handle2, &pool))
			.and_then(|e| e)
			.buffer_unordered(8)
			.or_else(|e| {
				println!("WARNING: user event error: {:?}", e);
				Ok(())
			}).for_each(|_| Ok(()))
		);
	}

	core.run(recv_term.map_err(|_| unreachable!()).and_then(|e| {
		match e {
			TerminalEvent::Exit(res) => Ok(res),
			TerminalEvent::Error(err) => Err(err),
		}
	}).into_future().map(|(e, _)| e.unwrap_or(0)).map_err(|(e, _)| e))
}

fn xmain(handle: &mut mpsc::Sender<XEvent>) -> Result<i32, Error> {
	let (conn, screen_num) = xcb::Connection::connect(None)?;
	let setup = conn.get_setup();
	let screen = setup.roots().nth(screen_num as usize).unwrap();

	let win = conn.generate_id();
	xcb::create_window(&conn,
		xcb::COPY_FROM_PARENT as u8,
		win,
		screen.root(),
		0, 0,
		150, 150,
		0,
		xcb::WINDOW_CLASS_INPUT_OUTPUT as u16,
		screen.root_visual(),
		&[
			(xcb::CW_BACK_PIXEL, screen.black_pixel()),
			(
				xcb::CW_EVENT_MASK,
				xcb::EVENT_MASK_VISIBILITY_CHANGE | xcb::EVENT_MASK_PROPERTY_CHANGE |
				xcb::EVENT_MASK_KEY_PRESS | xcb::EVENT_MASK_KEY_RELEASE |
				xcb::EVENT_MASK_BUTTON_PRESS | xcb::EVENT_MASK_BUTTON_RELEASE |
				xcb::EVENT_MASK_POINTER_MOTION | xcb::EVENT_MASK_BUTTON_MOTION |
				xcb::EVENT_MASK_STRUCTURE_NOTIFY | xcb::EVENT_MASK_FOCUS_CHANGE
			),
		]
	);
	xcb::map_window(&conn, win);
	conn.flush();

	let atom_wm_state = xcb::intern_atom(&conn, true, "WM_STATE").get_reply()?.atom();
	let atom_wm_protocols = xcb::intern_atom(&conn, true, "WM_PROTOCOLS").get_reply()?.atom();
	let atom_wm_delete_window = xcb::intern_atom(&conn, true, "WM_DELETE_WINDOW").get_reply()?.atom();

	xcb::change_property(&conn, xcb::PROP_MODE_REPLACE as _, win, atom_wm_protocols, 4, 32, &[atom_wm_delete_window]).request_check()?;

	let mut keys = xcb::get_keyboard_mapping(&conn, setup.min_keycode(), setup.max_keycode() - setup.min_keycode()).get_reply()?;
	let mut mods = xcb::get_modifier_mapping(&conn).get_reply()?;

	loop {
		let event = conn.wait_for_event();
		match event {
			None => break,
			Some(event) => {
				let r = event.response_type() & !0x80;
				match r {
					xcb::PROPERTY_NOTIFY => {
						let event: &xcb::PropertyNotifyEvent = unsafe {
							xcb::cast_event(&event)
						};

						match event.atom() {
							atom if atom == atom_wm_state => {
								let r = xcb::get_property(&conn, false, event.window(), event.atom(), 0, 0, 1).get_reply()?;
								let x = r.value::<u32>();
								let window_state_withdrawn = 0;
								// 1 is back but unobscured also works so ??
								let window_state_iconic = 3;
								match x.get(0) {
									Some(&state) if state == window_state_withdrawn || state == window_state_iconic => {
										send_event(handle, XEvent::Visible(false))?;
									},
									_ => (),
								}
							},
							_ => (),
						}
					},
					xcb::VISIBILITY_NOTIFY => {
						let event: &xcb::VisibilityNotifyEvent = unsafe {
							xcb::cast_event(&event)
						};

						match event.state() as _ {
							xcb::VISIBILITY_FULLY_OBSCURED => {
								send_event(handle, XEvent::Visible(false))?;
							},
							xcb::VISIBILITY_UNOBSCURED => {
								send_event(handle, XEvent::Visible(true))?;
							},
							_ => (),
						}
					},
					xcb::CLIENT_MESSAGE => {
						let event: &xcb::ClientMessageEvent = unsafe {
							xcb::cast_event(&event)
						};
						match event.data().data32().get(0) {
							Some(&atom) if atom == atom_wm_delete_window => {
								break
							},
							_ => (),
						}
					},
					xcb::BUTTON_PRESS => {
						let event: &xcb::ButtonPressEvent = unsafe {
							xcb::cast_event(&event)
						};
						send_event(handle, XEvent::Button {
							pressed: true,
							detail: event.detail(),
							state: event.state(),
						})?;
					},
					xcb::BUTTON_RELEASE => {
						let event: &xcb::ButtonReleaseEvent = unsafe {
							xcb::cast_event(&event)
						};
						send_event(handle, XEvent::Button {
							pressed: false,
							detail: event.detail(),
							state: event.state(),
						})?;
					},
					xcb::MOTION_NOTIFY => {
						let event: &xcb::MotionNotifyEvent = unsafe {
							xcb::cast_event(&event)
						};
						send_event(handle, XEvent::Mouse {
							x: event.event_x(),
							y: event.event_y(),
						})?;
					},
					xcb::FOCUS_OUT => {
						let event: &xcb::FocusOutEvent = unsafe {
							xcb::cast_event(&event)
						};
						send_event(handle, XEvent::Leave)?;
					},
					xcb::MAPPING_NOTIFY => {
						let event: &xcb::MappingNotifyEvent = unsafe {
							xcb::cast_event(&event)
						};
						keys = xcb::get_keyboard_mapping(&conn, setup.min_keycode(), setup.max_keycode() - setup.min_keycode()).get_reply()?;
						mods = xcb::get_modifier_mapping(&conn).get_reply()?;
					},
					xcb::FOCUS_IN => (),
					xcb::KEY_PRESS => {
						let event: &xcb::KeyPressEvent = unsafe {
							xcb::cast_event(&event)
						};
						let keycode = event.detail() - setup.min_keycode();
						let mut keysym = keys.keysyms().get(keycode as usize * keys.keysyms_per_keycode() as usize).map(Clone::clone);

						send_event(handle, XEvent::Key {
							pressed: true,
							keycode: keycode,
							keysym: if keysym == Some(0) { None } else { keysym },
							state: event.state(),
						})?;
					},
					xcb::KEY_RELEASE => {
						let event: &xcb::KeyReleaseEvent = unsafe {
							xcb::cast_event(&event)
						};
						let keycode = event.detail() - setup.min_keycode();
						let keysym = keys.keysyms().get(keycode as usize * keys.keysyms_per_keycode() as usize).map(Clone::clone);
						send_event(handle, XEvent::Key {
							pressed: false,
							keycode: keycode,
							keysym: if keysym == Some(0) { None } else { keysym },
							state: event.state(),
						})?;
					},
					xcb::CONFIGURE_NOTIFY => {
						let event: &xcb::ConfigureNotifyEvent = unsafe {
							xcb::cast_event(&event)
						};
						send_event(handle, XEvent::Resize {
							width: event.width(),
							height: event.height(),
						})?;
					},
					_ => {
						println!("unknown event {:}", r);
					},
				}
			}
		}
	}

	Ok(0)
}
