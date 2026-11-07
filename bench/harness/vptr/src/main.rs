// A virtual pointer for the headless checks. Stdin takes: "move X Y W H", "button CODE 0|1",
// "scroll LINES". Prints "ok" after each command, once the compositor has it.
use std::io::BufRead;
use wayland_client::protocol::{wl_pointer, wl_registry, wl_seat};
use wayland_client::{Connection, Dispatch, Proxy, QueueHandle};
use wayland_protocols_wlr::virtual_pointer::v1::client::{zwlr_virtual_pointer_manager_v1 as mgr, zwlr_virtual_pointer_v1 as vp};

#[derive(Default)]
struct S {
    seat: Option<wl_seat::WlSeat>,
    mgr: Option<mgr::ZwlrVirtualPointerManagerV1>,
}
impl Dispatch<wl_registry::WlRegistry, ()> for S {
    fn event(s: &mut S, r: &wl_registry::WlRegistry, e: wl_registry::Event, _: &(), _: &Connection, q: &QueueHandle<S>) {
        if let wl_registry::Event::Global { name, interface, version } = e {
            match interface.as_str() {
                "wl_seat" => s.seat = Some(r.bind(name, 1, q, ())),
                "zwlr_virtual_pointer_manager_v1" => s.mgr = Some(r.bind(name, version.min(2), q, ())),
                _ => {}
            }
        }
    }
}
macro_rules! noop { ($($t:ty),*) => { $(impl Dispatch<$t, ()> for S { fn event(_: &mut S, _: &$t, _: <$t as Proxy>::Event, _: &(), _: &Connection, _: &QueueHandle<S>) {} })* } }
noop!(wl_seat::WlSeat, mgr::ZwlrVirtualPointerManagerV1, vp::ZwlrVirtualPointerV1);

fn main() {
    let c = Connection::connect_to_env().expect("connect");
    let mut q = c.new_event_queue();
    let h = q.handle();
    let _r = c.display().get_registry(&h, ());
    let mut s = S::default();
    q.roundtrip(&mut s).unwrap();
    let p = s.mgr.as_ref().expect("no virtual pointer protocol").create_virtual_pointer(s.seat.as_ref(), &h, ());
    q.roundtrip(&mut s).unwrap();
    println!("ok");
    let mut t = 1u32;
    for line in std::io::stdin().lock().lines() {
        let line = line.unwrap();
        let f: Vec<&str> = line.split_whitespace().collect();
        t += 10;
        match f.as_slice() {
            ["move", x, y, w, ht] => p.motion_absolute(t, x.parse().unwrap(), y.parse().unwrap(), w.parse().unwrap(), ht.parse().unwrap()),
            ["button", code, st] => p.button(t, code.parse().unwrap(), if *st == "1" { wl_pointer::ButtonState::Pressed } else { wl_pointer::ButtonState::Released }),
            ["scroll", lines] => {
                let n: i32 = lines.parse().unwrap();
                p.axis_source(wl_pointer::AxisSource::Wheel);
                p.axis_discrete(t, wl_pointer::Axis::VerticalScroll, n as f64 * 15.0, n);
            }
            _ => continue,
        }
        p.frame();
        q.roundtrip(&mut s).unwrap();
        println!("ok");
    }
}
