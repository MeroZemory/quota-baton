#define WIN32_LEAN_AND_MEAN
#include <windows.h>
#include "plain_header_seh.h"

/* Do not hide destination faults, writes, execution or unrelated exceptions. */
static int qb_plain_header_read_filter(
    const EXCEPTION_POINTERS *fault, uintptr_t base, size_t view_len,
    uintptr_t required_byte)
{
    const EXCEPTION_RECORD *record;
    uintptr_t address;
    DWORD parameters;

    if (fault == NULL || fault->ExceptionRecord == NULL)
        return EXCEPTION_CONTINUE_SEARCH;
    record = fault->ExceptionRecord;
    if (record->ExceptionCode == EXCEPTION_IN_PAGE_ERROR)
        parameters = 3;
    else if (record->ExceptionCode == EXCEPTION_ACCESS_VIOLATION)
        parameters = 2;
    else
        return EXCEPTION_CONTINUE_SEARCH;
    if (record->NumberParameters != parameters ||
        record->ExceptionInformation[0] != 0) /* read access only */
        return EXCEPTION_CONTINUE_SEARCH;
    address = (uintptr_t)record->ExceptionInformation[1];
    /* Subtraction avoids base+length overflow; require this exact byte load. */
    if (address < base || address - base >= view_len ||
        address != required_byte)
        return EXCEPTION_CONTINUE_SEARCH;
    return EXCEPTION_EXECUTE_HANDLER;
}

uint32_t __cdecl qb_plain_header_copy_chunk(
    const void *view, size_t view_len, size_t offset,
    unsigned char *dst, size_t dst_len, size_t *written)
{
    uintptr_t base = (uintptr_t)view;
    uintptr_t destination = (uintptr_t)dst;
    volatile uintptr_t required_byte = 0;
    size_t count, i;

    if (written == NULL)
        return QB_PLAIN_HEADER_BAD_ARGUMENT;
    *written = 0;
    if (view_len > QB_PLAIN_HEADER_LIMIT || offset > view_len)
        return QB_PLAIN_HEADER_BAD_ARGUMENT;
    if (offset == view_len)
        return QB_PLAIN_HEADER_NO_NEWLINE;
    if (view == NULL || dst == NULL || dst_len == 0 ||
        base > UINTPTR_MAX - (view_len - 1))
        return QB_PLAIN_HEADER_BAD_ARGUMENT;
    count = view_len - offset;
    if (count > dst_len)
        count = dst_len;
    if (destination > UINTPTR_MAX - (count - 1))
        return QB_PLAIN_HEADER_BAD_ARGUMENT;
    if ((destination >= base && destination - base < view_len) ||
        (base >= destination && base - destination < count))
        return QB_PLAIN_HEADER_BAD_ARGUMENT;

    __try {
        for (i = 0; i < count; ++i) {
            unsigned char byte;
            /* One logical volatile load at a time; return at first newline. */
            required_byte = base + offset + i;
            byte = *(const volatile unsigned char *)required_byte;
            dst[i] = byte;
            if (byte == '\n') {
                *written = i + 1;
                return QB_PLAIN_HEADER_FOUND;
            }
        }
    }
    __except (qb_plain_header_read_filter(
        GetExceptionInformation(), base, view_len, required_byte)) {
        return QB_PLAIN_HEADER_MAPPED_FAULT;
    }

    if (offset + count == view_len)
        return QB_PLAIN_HEADER_NO_NEWLINE;
    *written = count;
    return QB_PLAIN_HEADER_MORE;
}
