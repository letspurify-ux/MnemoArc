use std::collections::BTreeMap;
#[derive(Default)]
pub struct Store { rows:BTreeMap<u64,String>, next:u64 }
impl Store {
    pub fn insert(&mut self,text:String)->u64 {self.next+=1;self.rows.insert(self.next,text);self.next}
    pub fn get(&self,id:u64)->Option<&str>{self.rows.get(&id).map(String::as_str)}
}
