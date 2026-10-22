//! Test-only PE/resource reader, independent of the build-time ICO encoder.
use std::{collections::BTreeMap, fs};

#[allow(dead_code)]
#[path = "../../crates/qb-tray/src/tray_icon/pixels.rs"]
mod pixels;

type Result<T> = std::result::Result<T, String>;

fn require(condition: bool, message: &str) -> Result<()> {
    if condition {
        Ok(())
    } else {
        Err(message.into())
    }
}

fn bytes(data: &[u8], offset: usize, length: usize) -> Result<&[u8]> {
    let end = offset.checked_add(length).ok_or("offset overflow")?;
    data.get(offset..end).ok_or_else(|| "truncated data".into())
}

fn u16_at(data: &[u8], offset: usize) -> Result<u16> {
    Ok(u16::from_le_bytes(
        bytes(data, offset, 2)?.try_into().unwrap(),
    ))
}

fn u32_at(data: &[u8], offset: usize) -> Result<u32> {
    Ok(u32::from_le_bytes(
        bytes(data, offset, 4)?.try_into().unwrap(),
    ))
}

struct Section {
    rva: usize,
    raw: usize,
    length: usize,
}

struct Pe<'a> {
    data: &'a [u8],
    sections: Vec<Section>,
    resource_rva: usize,
    resource_length: usize,
}

#[derive(Clone, Copy)]
struct Entry {
    name: u32,
    target: usize,
    directory: bool,
    name_offset: usize,
}

#[derive(Clone, Copy)]
struct Resource {
    offset: usize,
    length: usize,
    descriptor: usize,
}

struct Icons {
    group_name: usize,
    group: Resource,
    frames: Vec<(u32, Resource)>,
}

impl<'a> Pe<'a> {
    fn read(data: &'a [u8]) -> Result<Self> {
        require(bytes(data, 0, 2)? == b"MZ", "missing MZ")?;
        let pe = u32_at(data, 0x3c)? as usize;
        require(bytes(data, pe, 4)? == b"PE\0\0", "missing PE signature")?;
        let coff = pe + 4;
        let count = u16_at(data, coff + 2)? as usize;
        let optional_length = u16_at(data, coff + 16)? as usize;
        let optional = bytes(data, coff + 20, optional_length)?;
        let directories = match u16_at(optional, 0)? {
            0x10b => 96,
            0x20b => 112,
            _ => return Err("unsupported optional header".into()),
        };
        require(
            u32_at(optional, directories - 4)? >= 3,
            "missing resource directory",
        )?;
        let resource_rva = u32_at(optional, directories + 16)? as usize;
        let resource_length = u32_at(optional, directories + 20)? as usize;
        require(
            resource_rva != 0 && resource_length >= 16,
            "empty resources",
        )?;
        let table = coff + 20 + optional_length;
        let headers = bytes(data, table, count * 40)?;
        let mut sections = Vec::new();
        for header in headers.chunks_exact(40) {
            let section = Section {
                rva: u32_at(header, 12)? as usize,
                length: u32_at(header, 16)? as usize,
                raw: u32_at(header, 20)? as usize,
            };
            bytes(data, section.raw, section.length)?;
            sections.push(section);
        }
        let pe = Self {
            data,
            sections,
            resource_rva,
            resource_length,
        };
        pe.map_rva(resource_rva, resource_length)?;
        Ok(pe)
    }

    fn map_rva(&self, rva: usize, length: usize) -> Result<usize> {
        for section in &self.sections {
            if let Some(relative) = rva.checked_sub(section.rva)
                && relative
                    .checked_add(length)
                    .is_some_and(|end| end <= section.length)
            {
                let offset = section
                    .raw
                    .checked_add(relative)
                    .ok_or("file offset overflow")?;
                bytes(self.data, offset, length)?;
                return Ok(offset);
            }
        }
        Err("RVA outside file-backed sections".into())
    }

    fn resource_offset(&self, relative: usize, length: usize) -> Result<usize> {
        require(
            relative
                .checked_add(length)
                .is_some_and(|end| end <= self.resource_length),
            "resource outside directory bounds",
        )?;
        self.map_rva(
            self.resource_rva
                .checked_add(relative)
                .ok_or("resource RVA overflow")?,
            length,
        )
    }

    fn directory(&self, relative: usize) -> Result<Vec<Entry>> {
        let offset = self.resource_offset(relative, 16)?;
        let count = usize::from(u16_at(self.data, offset + 12)?)
            + usize::from(u16_at(self.data, offset + 14)?);
        let table = self.resource_offset(relative + 16, count * 8)?;
        let mut entries = Vec::new();
        for index in 0..count {
            let entry = table + index * 8;
            let target = u32_at(self.data, entry + 4)?;
            entries.push(Entry {
                name: u32_at(self.data, entry)?,
                target: (target & 0x7fff_ffff) as usize,
                directory: target & 0x8000_0000 != 0,
                name_offset: entry,
            });
        }
        Ok(entries)
    }

    fn resources(&self, kind: u32) -> Result<(usize, BTreeMap<u32, Resource>)> {
        let roots = self.directory(0)?;
        let matching: Vec<_> = roots.iter().filter(|entry| entry.name == kind).collect();
        require(matching.len() == 1, "missing/duplicate icon resource type")?;
        let root = matching[0];
        require(root.directory, "resource type is not a directory")?;
        let mut resources = BTreeMap::new();
        for entry in self.directory(root.target)? {
            require(
                entry.directory && entry.name <= u16::MAX.into(),
                "invalid icon ID",
            )?;
            let languages = self.directory(entry.target)?;
            require(languages.len() == 1, "expected exactly one icon language")?;
            let language = languages[0];
            require(
                language.name == 0 && !language.directory,
                "expected neutral icon data",
            )?;
            let descriptor = self.resource_offset(language.target, 16)?;
            let rva = u32_at(self.data, descriptor)? as usize;
            let length = u32_at(self.data, descriptor + 4)? as usize;
            let relative = rva
                .checked_sub(self.resource_rva)
                .ok_or("icon RVA before resources")?;
            let offset = self.resource_offset(relative, length)?;
            require(
                resources
                    .insert(
                        entry.name,
                        Resource {
                            offset,
                            length,
                            descriptor,
                        },
                    )
                    .is_none(),
                "duplicate icon ID",
            )?;
        }
        Ok((root.name_offset, resources))
    }
}

fn frames(data: &[u8]) -> Result<Icons> {
    let pe = Pe::read(data)?;
    let (group_name, groups) = pe.resources(14)?;
    require(
        groups.len() == 1 && groups.contains_key(&1),
        "expected application group 1 only",
    )?;
    let group = groups[&1];
    let directory = bytes(data, group.offset, group.length)?;
    require(
        u16_at(directory, 0)? == 0 && u16_at(directory, 2)? == 1,
        "invalid icon group",
    )?;
    require(
        u16_at(directory, 4)? == 4 && directory.len() == 6 + 4 * 14,
        "expected four frames",
    )?;
    let (_, mut icons) = pe.resources(3)?;
    let mut frames = BTreeMap::new();
    for entry in directory[6..].chunks_exact(14) {
        let size = u32::from(entry[0]);
        require([16, 20, 24, 32].contains(&size), "unsupported frame size")?;
        require(
            entry[0] == entry[1] && entry[2] == 0 && entry[3] == 0,
            "invalid frame dimensions/palette",
        )?;
        require(
            u16_at(entry, 4)? == 1 && u16_at(entry, 6)? == 32,
            "invalid frame format",
        )?;
        let id = u32::from(u16_at(entry, 12)?);
        let icon = icons
            .remove(&id)
            .ok_or("missing/reused RT_ICON reference")?;
        require(
            u32_at(entry, 8)? as usize == icon.length,
            "group/image length mismatch",
        )?;
        require(frames.insert(size, icon).is_none(), "duplicate frame size")?;
    }
    require(icons.is_empty(), "unreferenced icon resources")?;
    Ok(Icons {
        group_name,
        group,
        frames: frames.into_iter().collect(),
    })
}

fn compare_dib(data: &[u8], size: u32) -> Result<()> {
    let side = size as usize;
    let xor_length = side * side * 4;
    let mask_stride = side.div_ceil(32) * 4;
    require(
        data.len() == 40 + xor_length + side * mask_stride,
        "DIB length/trailing bytes",
    )?;
    require(u32_at(data, 0)? == 40, "expected BITMAPINFOHEADER")?;
    require(
        u32_at(data, 4)? == size && u32_at(data, 8)? == size * 2,
        "DIB dimensions/orientation",
    )?;
    require(
        u16_at(data, 12)? == 1 && u16_at(data, 14)? == 32,
        "DIB planes/depth",
    )?;
    require(u32_at(data, 16)? == 0, "expected BI_RGB")?;
    require(u32_at(data, 20)? as usize == xor_length, "DIB image size")?;
    require(
        data[24..40].iter().all(|byte| *byte == 0),
        "unexpected DIB metadata/palette",
    )?;
    let expected = pixels::render(size, false, pixels::Badge::Normal).ok_or("generator size")?;
    require(expected.size == size, "generator dimensions")?;
    let mut opaque = false;
    let mut partial = false;
    for y in 0..side {
        let stored_y = side - 1 - y;
        let mask = &data[40 + xor_length + stored_y * mask_stride..][..mask_stride];
        for x in 0..side {
            let actual = &data[40 + (stored_y * side + x) * 4..][..4];
            let wanted = &expected.bgra[(y * side + x) * 4..][..4];
            let alpha = wanted[3];
            require(actual[3] == alpha, "alpha mismatch")?;
            opaque |= alpha == 255;
            partial |= alpha > 0 && alpha < 255;
            for channel in 0..3 {
                // Compute independently in floating point, not through the encoder.
                let straight = if alpha == 0 {
                    0
                } else {
                    (f64::from(wanted[channel]) * 255.0 / f64::from(alpha))
                        .round()
                        .min(255.0) as u8
                };
                require(
                    actual[channel] == straight,
                    "straight-alpha channel mismatch",
                )?;
                let restored =
                    (f64::from(actual[channel]) * f64::from(alpha) / 255.0).round() as u8;
                require(
                    restored.abs_diff(wanted[channel]) <= 1,
                    "premultiplied channel mismatch",
                )?;
            }
            let transparent = mask[x / 8] & (0x80 >> (x % 8)) != 0;
            require(transparent == (alpha == 0), "AND mask mismatch")?;
            if (x == 0 || x == side - 1) && (y == 0 || y == side - 1) {
                require(actual == [0, 0, 0, 0], "nontransparent corner")?;
            }
        }
        for bit in side..mask_stride * 8 {
            require(
                mask[bit / 8] & (0x80 >> (bit % 8)) == 0,
                "nonzero mask padding",
            )?;
        }
    }
    require(opaque && partial, "missing opaque/partial-alpha samples")
}

fn verify(data: &[u8]) -> Result<()> {
    for (size, resource) in frames(data)?.frames {
        compare_dib(bytes(data, resource.offset, resource.length)?, size)
            .map_err(|error| format!("{size}px: {error}"))?;
    }
    Ok(())
}

pub fn assert_executable(path: &str) {
    let data = fs::read(path).expect("read final executable directly (no copy)");
    verify(&data).unwrap_or_else(|error| panic!("{path}: {error}"));
}

pub fn assert_rejects_corruption(path: &str) {
    let data = fs::read(path).expect("read final executable directly (no copy)");
    verify(&data).expect("uncorrupted final PE must pass first");
    let Icons {
        group_name,
        group,
        frames,
    } = frames(&data).unwrap();
    let (size, image) = frames[0];
    let reject = |name: &str, offset: usize, replacement: &[u8]| {
        let mut corrupted = data.clone(); // In-memory only; never copy a test executable to disk.
        corrupted[offset..offset + replacement.len()].copy_from_slice(replacement);
        assert!(verify(&corrupted).is_err(), "accepted {name}");
    };
    reject("missing group", group_name, &15_u32.to_le_bytes());
    let (_, images) = Pe::read(&data).unwrap().resources(3).unwrap();
    let missing_id = (0..=u16::MAX)
        .find(|id| !images.contains_key(&u32::from(*id)))
        .unwrap();
    reject(
        "missing image reference",
        group.offset + 6 + 12,
        &missing_id.to_le_bytes(),
    );
    reject(
        "out-of-range resource RVA",
        image.descriptor,
        &u32::MAX.to_le_bytes(),
    );
    reject(
        "out-of-range resource length",
        image.descriptor + 4,
        &u32::MAX.to_le_bytes(),
    );
    reject(
        "pixel corruption",
        image.offset + 40,
        &[data[image.offset + 40] ^ 1],
    );
    reject(
        "alpha corruption",
        image.offset + 43,
        &[data[image.offset + 43] ^ 1],
    );
    let mask = image.offset + 40 + (size * size * 4) as usize;
    reject("AND-mask corruption", mask, &[data[mask] ^ 0x80]);
    let padding_frame = frames.iter().find(|(size, _)| *size == 20).unwrap().1;
    let padding = padding_frame.offset + 40 + 20 * 20 * 4 + 3;
    reject("AND-mask padding corruption", padding, &[data[padding] ^ 1]);
}
