use std::time::Duration;

/// Time since the last keyboard/mouse input anywhere on the system, or `None` when this
/// desktop exposes no idle source. Blocking; call off the UI thread and async runtime.
#[cfg(target_os = "windows")]
#[allow(unsafe_code)]
pub fn system_idle() -> Option<Duration> {
	use windows::Win32::{
		System::SystemInformation::GetTickCount,
		UI::Input::KeyboardAndMouse::{GetLastInputInfo, LASTINPUTINFO},
	};
	let mut info = LASTINPUTINFO {
		cbSize: size_of::<LASTINPUTINFO>() as u32,
		dwTime: 0,
	};
	// SAFETY: stack LASTINPUTINFO with cbSize set; failure leaves dwTime at zero.
	let now = unsafe {
		let _ = GetLastInputInfo(&mut info);
		GetTickCount()
	};
	if info.dwTime == 0 {
		return None;
	}
	// Both are 32-bit millisecond tick counts that wrap together every ~49.7 days.
	Some(Duration::from_millis(u64::from(
		now.wrapping_sub(info.dwTime),
	)))
}

#[cfg(target_os = "macos")]
#[allow(unsafe_code)]
pub fn system_idle() -> Option<Duration> {
	#[link(name = "CoreGraphics", kind = "framework")]
	unsafe extern "C" {
		fn CGEventSourceSecondsSinceLastEventType(state: i32, event: u32) -> f64;
	}
	// SAFETY: pure query taking two integers; state 0 is the combined session, !0 any input.
	let seconds = unsafe { CGEventSourceSecondsSinceLastEventType(0, !0) };
	// Rejects negative, NaN and infinite values.
	Duration::try_from_secs_f64(seconds).ok()
}

#[cfg(target_os = "linux")]
pub fn system_idle() -> Option<Duration> {
	session_bus_idle(
		"org.gnome.Mutter.IdleMonitor",
		"/org/gnome/Mutter/IdleMonitor/Core",
		"org.gnome.Mutter.IdleMonitor",
		"GetIdletime",
	)
	.or_else(wayland::idle)
	.or_else(|| {
		session_bus_idle(
			"org.freedesktop.ScreenSaver",
			"/org/freedesktop/ScreenSaver",
			"org.freedesktop.ScreenSaver",
			"GetSessionIdleTime",
		)
	})
	.or_else(x11_idle)
}

/// ext-idle-notify-v1 for compositors without a D-Bus idle service (KDE Wayland, wlroots, COSMIC).
#[cfg(target_os = "linux")]
mod wayland {
	use std::{
		sync::{Arc, Mutex, OnceLock, PoisonError},
		time::{Duration, Instant},
	};
	use wayland_client::{
		Connection, Dispatch, QueueHandle, delegate_noop,
		protocol::{wl_registry, wl_seat::WlSeat},
	};
	use wayland_protocols::ext::idle_notify::v1::client::{
		ext_idle_notification_v1::{self as notification, ExtIdleNotificationV1},
		ext_idle_notifier_v1::ExtIdleNotifierV1,
	};

	/// The compositor only reports crossing this, so shorter idle times read as zero.
	const THRESHOLD: Duration = Duration::from_secs(60);

	enum Status {
		Active,
		Idled(Instant),
		Gone,
	}

	type Shared = Arc<Mutex<Status>>;

	struct State {
		status: Shared,
		seat: Option<WlSeat>,
		notifier: Option<ExtIdleNotifierV1>,
	}

	pub(super) fn idle() -> Option<Duration> {
		// Set up once; an unsupported compositor is not retried on every poll.
		static SHARED: OnceLock<Option<Shared>> = OnceLock::new();
		std::env::var_os("WAYLAND_DISPLAY")?;
		let shared = SHARED.get_or_init(start).as_ref()?;
		match *shared.lock().unwrap_or_else(PoisonError::into_inner) {
			Status::Active => Some(Duration::ZERO),
			// `idled` fires THRESHOLD after the last input.
			Status::Idled(since) => Some(THRESHOLD + since.elapsed()),
			Status::Gone => None,
		}
	}

	fn start() -> Option<Shared> {
		let connection = Connection::connect_to_env().ok()?;
		let mut queue = connection.new_event_queue::<State>();
		let qh = queue.handle();
		let status = Arc::new(Mutex::new(Status::Active));
		let mut state = State {
			status: status.clone(),
			seat: None,
			notifier: None,
		};
		let _registry = connection.display().get_registry(&qh, ());
		queue.roundtrip(&mut state).ok()?;
		let (seat, notifier) = (state.seat.take()?, state.notifier.take()?);
		notifier.get_idle_notification(THRESHOLD.as_millis() as u32, &seat, &qh, ());
		std::thread::Builder::new()
			.name("wayland-idle".into())
			.spawn(move || {
				let _connection = connection;
				while queue.blocking_dispatch(&mut state).is_ok() {}
				*state.status.lock().unwrap_or_else(PoisonError::into_inner) = Status::Gone;
			})
			.ok()?;
		Some(status)
	}

	impl Dispatch<wl_registry::WlRegistry, ()> for State {
		fn event(
			state: &mut Self,
			registry: &wl_registry::WlRegistry,
			event: wl_registry::Event,
			_: &(),
			_: &Connection,
			qh: &QueueHandle<Self>,
		) {
			if let wl_registry::Event::Global {
				name, interface, ..
			} = event
			{
				if interface == "wl_seat" && state.seat.is_none() {
					state.seat = Some(registry.bind(name, 1, qh, ()));
				} else if interface == "ext_idle_notifier_v1" && state.notifier.is_none() {
					state.notifier = Some(registry.bind(name, 1, qh, ()));
				}
			}
		}
	}

	impl Dispatch<ExtIdleNotificationV1, ()> for State {
		fn event(
			state: &mut Self,
			_: &ExtIdleNotificationV1,
			event: notification::Event,
			_: &(),
			_: &Connection,
			_: &QueueHandle<Self>,
		) {
			let next = match event {
				notification::Event::Idled => Status::Idled(Instant::now()),
				notification::Event::Resumed => Status::Active,
				_ => return,
			};
			*state.status.lock().unwrap_or_else(PoisonError::into_inner) = next;
		}
	}
	delegate_noop!(State: ignore WlSeat);
	delegate_noop!(State: ignore ExtIdleNotifierV1);
}

/// Both D-Bus idle services answer in milliseconds; Mutter as u64, KDE as u32.
#[cfg(target_os = "linux")]
fn session_bus_idle(
	destination: &'static str,
	path: &'static str,
	interface: &'static str,
	method: &'static str,
) -> Option<Duration> {
	use std::sync::OnceLock;
	static BUS: OnceLock<zbus::blocking::Connection> = OnceLock::new();
	let bus = match BUS.get() {
		Some(bus) => bus,
		None => BUS.get_or_init({
			// zbus waits forever by default; a hung service must not freeze the poller.
			let connection = zbus::blocking::connection::Builder::session()
				.ok()?
				.method_timeout(Duration::from_secs(2))
				.build()
				.ok()?;
			move || connection
		}),
	};
	let reply = bus
		.call_method(Some(destination), path, Some(interface), method, &())
		.ok()?;
	let body = reply.body();
	let millis = body
		.deserialize::<u64>()
		.or_else(|_| body.deserialize::<u32>().map(u64::from))
		.ok()?;
	Some(Duration::from_millis(millis))
}

#[cfg(target_os = "linux")]
#[allow(unsafe_code)]
// `c_ulong` is u64 here but u32 on 32-bit Linux, where the conversion is needed.
#[allow(clippy::useless_conversion)]
fn x11_idle() -> Option<Duration> {
	use x11_dl::{xlib, xss};
	// XWayland only sees X clients, so it would report native Wayland input as idle.
	if std::env::var_os("WAYLAND_DISPLAY").is_some() {
		return None;
	}
	std::env::var_os("DISPLAY")?;
	let xlib = xlib::Xlib::open().ok()?;
	let xss = xss::Xss::open().ok()?;
	// SAFETY: the display is opened and closed here; the info block is checked for null and
	// freed with XFree before returning. Libraries stay loaded for the whole block.
	unsafe {
		let display = (xlib.XOpenDisplay)(std::ptr::null());
		if display.is_null() {
			return None;
		}
		let info = (xss.XScreenSaverAllocInfo)();
		let idle = if info.is_null() {
			None
		} else {
			let root = (xlib.XDefaultRootWindow)(display);
			let ok = (xss.XScreenSaverQueryInfo)(display, root, info) != 0;
			let idle = ok.then(|| (*info).idle);
			(xlib.XFree)(info.cast());
			idle
		};
		(xlib.XCloseDisplay)(display);
		idle.map(|millis| Duration::from_millis(u64::from(millis)))
	}
}

#[cfg(not(any(target_os = "windows", target_os = "macos", target_os = "linux")))]
pub fn system_idle() -> Option<Duration> {
	None
}
