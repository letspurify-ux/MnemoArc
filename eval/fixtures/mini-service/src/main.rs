mod store;
mod service;
use service::Service;
fn main() {
    let mut service=Service::default();
    match service.create("first task") {
        Ok(id)=>println!("Created {id}: {}",service.get(id).unwrap()),
        Err(error)=>eprintln!("Creation failed: {error}"),
    }
}
