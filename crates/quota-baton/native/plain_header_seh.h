#ifndef QB_PLAIN_HEADER_SEH_H
#define QB_PLAIN_HEADER_SEH_H

#if !defined(_WIN32) || !defined(_MSC_VER) || defined(__cplusplus)
#error This helper requires Windows MSVC C and structured exception handling.
#endif

#include <stddef.h>
#include <stdint.h>

#define QB_PLAIN_HEADER_LIMIT ((size_t)4 * 1024 * 1024)

enum {
    QB_PLAIN_HEADER_FOUND = 0,
    QB_PLAIN_HEADER_MORE = 1,
    QB_PLAIN_HEADER_NO_NEWLINE = 2,
    QB_PLAIN_HEADER_MAPPED_FAULT = 3,
    QB_PLAIN_HEADER_BAD_ARGUMENT = 4
};

/*
 * view is a retained mapping, view_len <= QB_PLAIN_HEADER_LIMIT. offset is
 * the length already copied. dst is disjoint, caller-owned writable storage;
 * written points to a separate writable size_t. No pointer is retained.
 *
 * Only FOUND/MORE publish initialized bytes in written. Other statuses leave
 * written zero; discard any partial destination. Only source reads are inside
 * the exception filter's scope. No allocation, callback or Rust unwind occurs
 * across the SEH region. Rust allocates and frees the owned output.
 */
uint32_t __cdecl qb_plain_header_copy_chunk(
    const void *view, size_t view_len, size_t offset,
    unsigned char *dst, size_t dst_len, size_t *written);

#endif
