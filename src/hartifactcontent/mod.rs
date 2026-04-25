mod unpack_file;
mod unpack_raw;
pub mod pack_tar;
mod unpack_tar;
pub mod unpack;

#[derive(Clone)]
pub struct ContentRaw {
    pub data: Vec<u8>,
    pub path: String,
    pub x: bool,
}

#[derive(Clone)]
pub struct ContentFile {
    pub source_path: String,
    pub out_path: String,
    pub x: bool,
}

#[derive(Clone)]
pub enum Content {
    File(ContentFile),
    Raw(ContentRaw),
    TarPath(String),
    CpioPath(String),
}
