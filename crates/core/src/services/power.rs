//! Shutting the Pi down, as the car's ignition goes off: through
//! systemd-logind, which lets the service's account do it under NavBridge's
//! polkit rule.
use zbus::blocking::Connection;

pub fn power_off() -> Result<(), String> {
    let connection = Connection::system().map_err(|e| e.to_string())?;
    // Not interactive: nobody is there to type a password.
    connection
        .call_method(
            Some("org.freedesktop.login1"),
            "/org/freedesktop/login1",
            Some("org.freedesktop.login1.Manager"),
            "PowerOff",
            &(false,),
        )
        .map(|_| ())
        .map_err(|e| e.to_string())
}
