use crate::{
    binary::{level_4_entries::UsedLevel4Entries, PAGE_SIZE},
    boot_info::TlsTemplate,
};
use x86_64::{
    align_up,
    structures::paging::{
        mapper::MapperAllSizes, FrameAllocator, Page, PageSize, PageTableFlags as Flags, PhysFrame,
        Size4KiB,
    },
    PhysAddr, VirtAddr,
};
use xmas_elf::{
    dynamic, header,
    program::{self, ProgramHeader, SegmentData, Type},
    sections::Rela,
    ElfFile,
};

struct Loader<'a, M, F> {
    elf_file: ElfFile<'a>,
    inner: Inner<'a, M, F>,
}

struct Inner<'a, M, F> {
    kernel_offset: PhysAddr,
    virtual_address_offset: u64,
    page_table: &'a mut M,
    frame_allocator: &'a mut F,
}

impl<'a, M, F> Loader<'a, M, F>
where
    M: MapperAllSizes,
    F: FrameAllocator<Size4KiB>,
{
    fn new(
        bytes: &'a [u8],
        page_table: &'a mut M,
        frame_allocator: &'a mut F,
    ) -> Result<Self, &'static str> {
        log::info!("Elf file loaded at {:#p}", bytes);
        let kernel_offset = PhysAddr::new(&bytes[0] as *const u8 as u64);
        if !kernel_offset.is_aligned(PAGE_SIZE) {
            return Err("Loaded kernel ELF file is not sufficiently aligned");
        }

        let elf_file = ElfFile::new(bytes)?;

        let virtual_address_offset = match elf_file.header.pt2.type_().as_type() {
            header::Type::None => unimplemented!(),
            header::Type::Relocatable => unimplemented!(),
            header::Type::Executable => 0,
            header::Type::SharedObject => 0x400000,
            header::Type::Core => unimplemented!(),
            header::Type::ProcessorSpecific(_) => unimplemented!(),
        };

        header::sanity_check(&elf_file)?;
        let loader = Loader {
            elf_file,
            inner: Inner {
                kernel_offset,
                virtual_address_offset,
                page_table,
                frame_allocator,
            },
        };

        Ok(loader)
    }

    fn load_segments(&mut self) -> Result<Option<TlsTemplate>, &'static str> {
        for program_header in self.elf_file.program_iter() {
            program::sanity_check(program_header, &self.elf_file)?;
        }

        // Apply relocations in physical memory.
        for program_header in self.elf_file.program_iter() {
            if let Type::Dynamic = program_header.get_type()? {
                self.inner
                    .handle_dynamic_segment(program_header, &self.elf_file)?
            }
        }

        // Load the segments into virtual memory.
        let mut tls_template = None;
        for program_header in self.elf_file.program_iter() {
            match program_header.get_type()? {
                Type::Load => self.inner.handle_load_segment(program_header)?,
                Type::Tls => {
                    if tls_template.is_none() {
                        tls_template = Some(self.inner.handle_tls_segment(program_header)?);
                    } else {
                        return Err("multiple TLS segments not supported");
                    }
                }
                Type::Null
                | Type::Dynamic
                | Type::Interp
                | Type::Note
                | Type::ShLib
                | Type::Phdr
                | Type::GnuRelro
                | Type::OsSpecific(_)
                | Type::ProcessorSpecific(_) => {}
            }
        }
        Ok(tls_template)
    }

    fn entry_point(&self) -> VirtAddr {
        VirtAddr::new(self.elf_file.header.pt2.entry_point() + self.inner.virtual_address_offset)
    }

    fn used_level_4_entries(&self) -> UsedLevel4Entries {
        UsedLevel4Entries::new(
            self.elf_file.program_iter(),
            self.inner.virtual_address_offset,
        )
    }
}

impl<'a, M, F> Inner<'a, M, F>
where
    M: MapperAllSizes,
    F: FrameAllocator<Size4KiB>,
{
    fn handle_load_segment(&mut self, segment: ProgramHeader) -> Result<(), &'static str> {
        log::info!("Handling Segment: {:x?}", segment);

        let phys_start_addr = self.kernel_offset + segment.offset();
        let start_frame: PhysFrame = PhysFrame::containing_address(phys_start_addr);
        let end_frame: PhysFrame =
            PhysFrame::containing_address(phys_start_addr + segment.file_size() - 1u64);

        let virt_start_addr = VirtAddr::new(segment.virtual_addr()) + self.virtual_address_offset;
        let start_page: Page = Page::containing_address(virt_start_addr);

        let mut segment_flags = Flags::PRESENT;
        if !segment.flags().is_execute() {
            segment_flags |= Flags::NO_EXECUTE;
        }
        if segment.flags().is_write() {
            segment_flags |= Flags::WRITABLE;
        }

        // map all frames of the segment at the desired virtual address
        for frame in PhysFrame::range_inclusive(start_frame, end_frame) {
            let offset = frame - start_frame;
            let page = start_page + offset;
            let flusher = unsafe {
                self.page_table
                    .map_to(page, frame, segment_flags, self.frame_allocator)
                    .map_err(|_err| "map_to failed")?
            };
            // we operate on an inactive page table, so there's no need to flush anything
            flusher.ignore();
        }

        // Handle .bss section (mem_size > file_size)
        if segment.mem_size() > segment.file_size() {
            // .bss section (or similar), which needs to be mapped and zeroed
            self.handle_bss_section(&segment, segment_flags)?;
        }

        Ok(())
    }

    fn handle_bss_section(
        &mut self,
        segment: &ProgramHeader,
        segment_flags: Flags,
    ) -> Result<(), &'static str> {
        log::info!("Mapping bss section");

        let virt_start_addr = VirtAddr::new(segment.virtual_addr()) + self.virtual_address_offset;
        let phys_start_addr = self.kernel_offset + segment.offset();
        let mem_size = segment.mem_size();
        let file_size = segment.file_size();

        // calculate virual memory region that must be zeroed
        let zero_start = virt_start_addr + file_size;
        let zero_end = virt_start_addr + mem_size;

        // a type alias that helps in efficiently clearing a page
        type PageArray = [u64; Size4KiB::SIZE as usize / 8];
        const ZERO_ARRAY: PageArray = [0; Size4KiB::SIZE as usize / 8];

        // In some cases, `zero_start` might not be page-aligned. This requires some
        // special treatment because we can't safely zero a frame of the original file.
        let data_bytes_before_zero = zero_start.as_u64() & 0xfff;
        if data_bytes_before_zero != 0 {
            // The last non-bss frame of the segment consists partly of data and partly of bss
            // memory, which must be zeroed. Unfortunately, the file representation might have
            // reused the part of the frame that should be zeroed to store the next segment. This
            // means that we can't simply overwrite that part with zeroes, as we might overwrite
            // other data this way.
            //
            // Example:
            //
            //   XXXXXXXXXXXXXXX000000YYYYYYY000ZZZZZZZZZZZ     virtual memory (XYZ are data)
            //   |·············|     /·····/   /·········/
            //   |·············| ___/·····/   /·········/
            //   |·············|/·····/‾‾‾   /·········/
            //   |·············||·····|/·̅·̅·̅·̅·̅·····/‾‾‾‾
            //   XXXXXXXXXXXXXXXYYYYYYYZZZZZZZZZZZ              file memory (zeros are not saved)
            //   '       '       '       '        '
            //   The areas filled with dots (`·`) indicate a mapping between virtual and file
            //   memory. We see that the data regions `X`, `Y`, `Z` have a valid mapping, while
            //   the regions that are initialized with 0 have not.
            //
            //   The ticks (`'`) below the file memory line indicate the start of a new frame. We
            //   see that the last frames of the `X` and `Y` regions in the file are followed
            //   by the bytes of the next region. So we can't zero these parts of the frame
            //   because they are needed by other memory regions.
            //
            // To solve this problem, we need to allocate a new frame for the last segment page
            // and copy all data content of the original frame over. Afterwards, we can zero
            // the remaining part of the frame since the frame is no longer shared with other
            // segments now.

            // calculate the frame where the last segment page is mapped
            let orig_frame: PhysFrame =
                PhysFrame::containing_address(phys_start_addr + file_size - 1u64);
            // allocate a new frame to replace `orig_frame`
            let new_frame = self.frame_allocator.allocate_frame().unwrap();

            // zero new frame, utilizing that it's identity-mapped
            {
                let new_frame_ptr = new_frame.start_address().as_u64() as *mut PageArray;
                unsafe { new_frame_ptr.write(ZERO_ARRAY) };
            }

            // copy the data bytes from orig_frame to new_frame
            {
                log::info!("Copy contents");
                let orig_bytes_ptr = orig_frame.start_address().as_u64() as *mut u8;
                let new_bytes_ptr = new_frame.start_address().as_u64() as *mut u8;

                for offset in 0..(data_bytes_before_zero as isize) {
                    unsafe {
                        let orig_byte = orig_bytes_ptr.offset(offset).read();
                        new_bytes_ptr.offset(offset).write(orig_byte);
                    }
                }
            }

            // remap last page from orig_frame to `new_frame`
            log::info!("Remap last page");
            let last_page = Page::containing_address(virt_start_addr + file_size - 1u64);
            self.page_table
                .unmap(last_page.clone())
                .map_err(|_err| "Failed to unmap last segment page because of bss memory")?
                .1
                .ignore();
            let flusher = unsafe {
                self.page_table
                    .map_to(last_page, new_frame, segment_flags, self.frame_allocator)
            }
            .map_err(|_err| "Failed to remap last segment page because of bss memory")?;
            // we operate on an inactive page table, so we don't need to flush our changes
            flusher.ignore();
        }

        // map additional frames for `.bss` memory that is not present in source file
        let start_page: Page =
            Page::containing_address(VirtAddr::new(align_up(zero_start.as_u64(), Size4KiB::SIZE)));
        let end_page = Page::containing_address(zero_end);
        for page in Page::range_inclusive(start_page, end_page) {
            // allocate a new unused frame
            let frame = self.frame_allocator.allocate_frame().unwrap();

            // zero frame, utilizing identity-mapping
            let frame_ptr = frame.start_address().as_u64() as *mut PageArray;
            unsafe { frame_ptr.write(ZERO_ARRAY) };

            // map frame
            let flusher = unsafe {
                self.page_table
                    .map_to(page, frame, segment_flags, self.frame_allocator)
                    .map_err(|_err| "Failed to map new frame for bss memory")?
            };
            // we operate on an inactive page table, so we don't need to flush our changes
            flusher.ignore();
        }

        Ok(())
    }

    fn handle_tls_segment(&mut self, segment: ProgramHeader) -> Result<TlsTemplate, &'static str> {
        Ok(TlsTemplate {
            start_addr: segment.virtual_addr() + self.virtual_address_offset,
            mem_size: segment.mem_size(),
            file_size: segment.file_size(),
        })
    }

    fn handle_dynamic_segment(
        &mut self,
        segment: ProgramHeader,
        elf_file: &ElfFile,
    ) -> Result<(), &'static str> {
        let data = segment.get_data(elf_file)?;
        let data = if let SegmentData::Dynamic64(data) = data {
            data
        } else {
            unreachable!()
        };

        // Find the `Rela`, `RelaSize` and `RelaEnt` entries.
        let mut rela = None;
        let mut rela_size = None;
        let mut rela_ent = None;
        for rel in data {
            let tag = rel.get_tag()?;
            match tag {
                dynamic::Tag::Rela => {
                    let ptr = rel.get_ptr()?;
                    let prev = rela.replace(ptr);
                    if prev.is_some() {
                        return Err("Dynamic section contains more than one Rela entry");
                    }
                }
                dynamic::Tag::RelaSize => {
                    let val = rel.get_val()?;
                    let prev = rela_size.replace(val);
                    if prev.is_some() {
                        return Err("Dynamic section contains more than one RelaSize entry");
                    }
                }
                dynamic::Tag::RelaEnt => {
                    let val = rel.get_val()?;
                    let prev = rela_ent.replace(val);
                    if prev.is_some() {
                        return Err("Dynamic section contains more than one RelaEnt entry");
                    }
                }
                _ => {}
            }
        }
        let offset = if let Some(rela) = rela {
            rela
        } else {
            // The section doesn't contain any relocations.

            assert_eq!(rela_size, None);
            assert_eq!(rela_ent, None);

            return Ok(());
        };
        let total_size = rela_size.ok_or("RelaSize entry is missing")?;
        let entry_size = rela_ent.ok_or("RelaEnt entry is missing")?;

        // Apply the mappings.
        let entries = total_size / entry_size;
        let relas = unsafe {
            core::slice::from_raw_parts::<Rela<u64>>(
                elf_file.input.as_ptr().add(offset as usize).cast(),
                entries as usize,
            )
        };
        for rela in relas {
            let idx = rela.get_symbol_table_index();
            assert_eq!(
                idx, 0,
                "relocations using the symbol table are not supported"
            );

            match rela.get_type() {
                8 => {
                    let offset_in_file = find_offset(elf_file, rela.get_offset())?
                        .ok_or("Destination of relocation is not mapped in physical memory")?;
                    let dest_addr = self.kernel_offset + offset_in_file;
                    let dest_ptr = dest_addr.as_u64() as *mut u64;

                    let value = self
                        .virtual_address_offset
                        .checked_add(rela.get_addend())
                        .unwrap();

                    unsafe {
                        // write new value, utilizing that the address identity-mapped
                        dest_ptr.write(value);
                    }
                }
                ty => unimplemented!("relocation type {:x} not supported", ty),
            }
        }

        Ok(())
    }
}

/// Locate the offset into the elf file corresponding to a virtual address.
fn find_offset(elf_file: &ElfFile, virt_offset: u64) -> Result<Option<u64>, &'static str> {
    for program_header in elf_file.program_iter() {
        if let Type::Load = program_header.get_type()? {
            if program_header.virtual_addr() <= virt_offset {
                let offset_in_segment = virt_offset - program_header.virtual_addr();
                if offset_in_segment < program_header.file_size() {
                    return Ok(Some(program_header.offset() + offset_in_segment));
                }
            }
        }
    }
    Ok(None)
}

/// Loads the kernel ELF file given in `bytes` in the given `page_table`.
///
/// Returns the kernel entry point address, it's thread local storage template (if any),
/// and a structure describing which level 4 page table entries are in use.  
pub fn load_kernel(
    bytes: &[u8],
    page_table: &mut impl MapperAllSizes,
    frame_allocator: &mut impl FrameAllocator<Size4KiB>,
) -> Result<(VirtAddr, Option<TlsTemplate>, UsedLevel4Entries), &'static str> {
    let mut loader = Loader::new(bytes, page_table, frame_allocator)?;
    let tls_template = loader.load_segments()?;
    let used_entries = loader.used_level_4_entries();

    Ok((loader.entry_point(), tls_template, used_entries))
}
