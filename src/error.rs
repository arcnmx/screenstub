use xcb;

quick_error! {
	#[derive(Debug)]
	pub enum Error {
		Generic(err: xcb::GenericError) {
			from()
			cause(err)
			display("Generic error: {}", err)
		}
		Conn(err: xcb::ConnError) {
			from()
			cause(err)
			display("Connection error: {}", err)
		}
	}
}
