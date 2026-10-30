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
