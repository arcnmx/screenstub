extern crate xcb;
#[macro_use]
extern crate quick_error;

mod error;

use error::Error;
use std::process::exit;
use std::io;

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

	let res = Command::new("screenstub-handler")
		.arg(if visible { "on" } else { "off" })
		.spawn().map(drop);

	if let Err(ref err) = res {
		println!("Failed to spawn event handler: {:?}", err);
	}

	res
}

fn main_result() -> Result<i32, Error> {
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
		10,
		xcb::WINDOW_CLASS_INPUT_OUTPUT as u16,
		screen.root_visual(),
		&[
			(xcb::CW_BACK_PIXEL, screen.black_pixel()),
			(xcb::CW_EVENT_MASK, xcb::EVENT_MASK_VISIBILITY_CHANGE | xcb::EVENT_MASK_PROPERTY_CHANGE),
		]
	);
	xcb::map_window(&conn, win);
	conn.flush();

	let atom_wm_state = xcb::intern_atom(&conn, true, "WM_STATE").get_reply()?.atom();
	let atom_wm_protocols = xcb::intern_atom(&conn, true, "WM_PROTOCOLS").get_reply()?.atom();
	let atom_wm_delete_window = xcb::intern_atom(&conn, true, "WM_DELETE_WINDOW").get_reply()?.atom();

	xcb::change_property(&conn, xcb::PROP_MODE_REPLACE as _, win, atom_wm_protocols, 4, 32, &[atom_wm_delete_window]).request_check()?;

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
										handle_visible_event(false);
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
								handle_visible_event(false);
							},
							xcb::VISIBILITY_UNOBSCURED => {
								handle_visible_event(true);
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
								handle_visible_event(false);
								break
							},
							_ => (),
						}
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
