// Bake-off probe: wayland-client + wl_shm + swash, drawing a window of 100,000-row list rows.
use std::os::fd::AsFd;
use swash::{scale::{ScaleContext, Render, Source}, FontRef, zeno::Format};
use wayland_client::{protocol::*, *};
use wayland_protocols::xdg::shell::client::*;
#[derive(Default)] struct S { comp:Option<wl_compositor::WlCompositor>, shm:Option<wl_shm::WlShm>, wm:Option<xdg_wm_base::XdgWmBase>, serial:Option<u32> }
impl Dispatch<wl_registry::WlRegistry,()> for S { fn event(s:&mut S,r:&wl_registry::WlRegistry,e:wl_registry::Event,_:&(),_:&Connection,q:&QueueHandle<S>){ if let wl_registry::Event::Global{name,interface,version}=e { match &interface[..] { "wl_compositor"=>s.comp=Some(r.bind(name,version.min(4),q,())), "wl_shm"=>s.shm=Some(r.bind(name,1,q,())), "xdg_wm_base"=>s.wm=Some(r.bind(name,1,q,())), _=>{} } } } }
macro_rules! noop { ($($t:ty),*) => { $(impl Dispatch<$t,()> for S { fn event(_:&mut S,_:&$t,_:<$t as Proxy>::Event,_:&(),_:&Connection,_:&QueueHandle<S>){} })* } }
noop!(wl_compositor::WlCompositor,wl_shm::WlShm,wl_shm_pool::WlShmPool,wl_buffer::WlBuffer,wl_surface::WlSurface,xdg_toplevel::XdgToplevel);
impl Dispatch<xdg_wm_base::XdgWmBase,()> for S { fn event(_:&mut S,w:&xdg_wm_base::XdgWmBase,e:xdg_wm_base::Event,_:&(),_:&Connection,_:&QueueHandle<S>){ if let xdg_wm_base::Event::Ping{serial}=e { w.pong(serial) } } }
impl Dispatch<xdg_surface::XdgSurface,()> for S { fn event(s:&mut S,x:&xdg_surface::XdgSurface,e:xdg_surface::Event,_:&(),_:&Connection,_:&QueueHandle<S>){ if let xdg_surface::Event::Configure{serial}=e { x.ack_configure(serial); s.serial=Some(serial) } } }
fn main(){
  let names:Vec<String>=(0..100_000).map(|i|format!("file-{i}.txt")).collect();
  let path=std::env::args().nth(1).unwrap_or("/usr/share/fonts/TTF/JetBrainsMonoNerdFont-Regular.ttf".into());
  let data=std::fs::read(&path).expect("font"); let font=FontRef::from_index(&data,0).unwrap();
  let c=Connection::connect_to_env().unwrap(); let mut q=c.new_event_queue(); let h=q.handle(); let d=c.display(); let _r=d.get_registry(&h,()); let mut s=S::default(); q.roundtrip(&mut s).unwrap();
  let surf=s.comp.as_ref().unwrap().create_surface(&h,()); let xs=s.wm.as_ref().unwrap().get_xdg_surface(&surf,&h,()); let t=xs.get_toplevel(&h,()); t.set_title("rawshm".into()); surf.commit();
  while s.serial.is_none(){ q.blocking_dispatch(&mut s).unwrap(); }
  let (w,hh)=(1200i32,760i32); let size=(w*hh*4) as usize;
  let fd=rustix::fs::memfd_create("buf",rustix::fs::MemfdFlags::CLOEXEC).unwrap(); rustix::fs::ftruncate(&fd,size as u64).unwrap();
  let map=unsafe{ rustix::mm::mmap(std::ptr::null_mut(),size,rustix::mm::ProtFlags::READ|rustix::mm::ProtFlags::WRITE,rustix::mm::MapFlags::SHARED,&fd,0).unwrap() } as *mut u32;
  let px=unsafe{ std::slice::from_raw_parts_mut(map,(w*hh) as usize) }; px.fill(0xff101418);
  let mut ctx=ScaleContext::new(); let mut sc=ctx.builder(font).size(14.0).hint(true).build();
  let adv=font.glyph_metrics(&[]).scale(14.0); let cm=font.charmap();
  let mut cache=std::collections::HashMap::<u16,swash::scale::image::Image>::new();
  for (row,n) in names.iter().take(32).enumerate(){ let mut x=8.0f32; let base=row as i32*23+18; for ch in n.chars(){ let g=cm.map(ch);
     let img=cache.entry(g).or_insert_with(||Render::new(&[Source::Outline]).format(Format::Alpha).render(&mut sc,g).unwrap());
     let (l,tp,iw,ih)=(img.placement.left,img.placement.top,img.placement.width as i32,img.placement.height as i32);
     for yy in 0..ih { for xx in 0..iw { let a=img.data[(yy*iw+xx) as usize] as u32; if a==0 {continue} let (dx,dy)=(x as i32+l+xx,base-tp+yy); if dx<0||dy<0||dx>=w||dy>=hh {continue} let p=&mut px[(dy*w+dx) as usize]; let f=0xddu32; let b=*p&0xff; let v=(f*a+b*(255-a))/255; *p=0xff000000|v<<16|v<<8|v; } }
     x+=adv.advance_width(g); } }
  let pool=s.shm.as_ref().unwrap().create_pool(fd.as_fd(),size as i32,&h,()); let buf=pool.create_buffer(0,w,hh,w*4,wl_shm::Format::Xrgb8888,&h,());
  surf.attach(Some(&buf),0,0); surf.damage_buffer(0,0,w,hh); surf.commit();
  std::hint::black_box(&names);
  loop { q.blocking_dispatch(&mut s).unwrap(); }
}
