#include <stdio.h>
#include <stdint.h>
#include <stdbool.h>

#include <fuse3/fuse.h>

#define MAKE_ACCESSORS(name) \
	extern void glue_set_ffi_##name(struct fuse_file_info *ffi, unsigned int value) { \
		ffi->name = value; \
	} \
	extern unsigned int glue_get_ffi_##name(struct fuse_file_info *ffi) { \
		return ffi->name; \
	}

MAKE_ACCESSORS(writepage)
MAKE_ACCESSORS(direct_io)
MAKE_ACCESSORS(keep_cache)
MAKE_ACCESSORS(flush)
MAKE_ACCESSORS(nonseekable)
MAKE_ACCESSORS(flock_release)
MAKE_ACCESSORS(cache_readdir)
MAKE_ACCESSORS(noflush)
