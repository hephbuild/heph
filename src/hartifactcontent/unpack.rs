use std::path::Path;
use crate::hartifactcontent::Content;
use crate::hartifactcontent::unpack_file::unpack_file;
use crate::hartifactcontent::unpack_raw::unpack_raw;
use crate::hartifactcontent::unpack_tar::unpack_tar;

pub fn unpack(content: &Content, to: &Path) -> anyhow::Result<()> {
    match content {
        Content::File(file) => unpack_file(file, to),
        Content::Raw(raw) => unpack_raw(raw, to),
        Content::TarPath(path) => unpack_tar(path, to),
        Content::CpioPath(_) => unimplemented!("CpioPath unpack not yet implemented"),
    }
}
