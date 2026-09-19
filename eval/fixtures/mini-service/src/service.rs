use crate::store::Store;
#[derive(Default)]
pub struct Service {store:Store}
impl Service {
    pub fn create(&mut self,text:&str)->Result<u64,&'static str>{
        let text=text.trim();
        if text.is_empty(){return Err("empty task");}
        if text.len()>200{return Err("task exceeds 200 UTF-8 bytes");}
        Ok(self.store.insert(text.into()))
    }
    pub fn get(&self,id:u64)->Result<&str,&'static str>{self.store.get(id).ok_or("unknown task")}
}
