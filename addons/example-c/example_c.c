/*
 * example-c: a medicus addon written in plain C11 against include/medicus_addon.h.
 *
 * It provides an in-memory BlobStore ("memory") and an EventHook ("count-events"). It is
 * intentionally simple (a linked list, one lock) and exists to show, and to test in CI, that the
 * ABI is usable without Rust. The rules it follows are described in include/README.md.
 */

#include "medicus_addon.h"

#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#if defined(_WIN32)
#include <windows.h>
typedef SRWLOCK md_lock;
static void lock_init(md_lock *l) { InitializeSRWLock(l); }
static void lock_acquire(md_lock *l) { AcquireSRWLockExclusive(l); }
static void lock_release(md_lock *l) { ReleaseSRWLockExclusive(l); }
static void lock_destroy(md_lock *l) { (void)l; }
#else
#include <pthread.h>
typedef pthread_mutex_t md_lock;
static void lock_init(md_lock *l) { pthread_mutex_init(l, NULL); }
static void lock_acquire(md_lock *l) { pthread_mutex_lock(l); }
static void lock_release(md_lock *l) { pthread_mutex_unlock(l); }
static void lock_destroy(md_lock *l) { pthread_mutex_destroy(l); }
#endif

/* Must equal the host's `<arch>-<os>` (Rust's std::env::consts::ARCH / OS). */
#if defined(__x86_64__) || defined(_M_X64)
#define MD_ARCH "x86_64"
#elif defined(__aarch64__) || defined(_M_ARM64)
#define MD_ARCH "aarch64"
#else
#error "unsupported architecture"
#endif
#if defined(_WIN32)
#define MD_OS "windows"
#elif defined(__APPLE__)
#define MD_OS "macos"
#elif defined(__linux__)
#define MD_OS "linux"
#else
#error "unsupported operating system"
#endif

#define CHUNK_SIZE (64 * 1024)

static const MdHostApi *g_host;

/* ---- helpers ------------------------------------------------------------------------------ */

static MdStr str(const char *s) {
    MdStr r = {(const uint8_t *)s, strlen(s)};
    return r;
}

static void log_msg(uint32_t level, const char *msg) {
    g_host->log(g_host->ctx, level, str("example-c"), str(msg));
}

static void release_malloc(void *ctx, uint8_t *ptr, size_t len) {
    (void)ctx;
    (void)len;
    free(ptr);
}

static MdBuffer buffer_empty(void) {
    MdBuffer b = {NULL, 0, NULL, NULL};
    return b;
}

/* Copies `len` bytes into a malloc'ed buffer released with free(). Empty on failure. */
static MdBuffer buffer_copy(const void *data, size_t len) {
    MdBuffer b = buffer_empty();
    if (len == 0) return b;
    b.ptr = (uint8_t *)malloc(len);
    if (!b.ptr) return b;
    memcpy(b.ptr, data, len);
    b.len = len;
    b.release = release_malloc;
    return b;
}

static MdBuffer buffer_str(const char *s) { return buffer_copy(s, strlen(s)); }

static void buffer_free(MdBuffer *b) {
    if (b->release) b->release(b->ctx, b->ptr, b->len);
}

static void complete(MdCompletion done, MdStatus status, MdBuffer result) {
    done.complete(done.ctx, status, result);
}

static void complete_msg(MdCompletion done, MdStatus status, const char *msg) {
    complete(done, status, buffer_str(msg));
}

/* Copies a borrowed MdStr into a NUL-terminated string (keys must outlive the call). */
static char *dup_str(MdStr s) {
    char *r = (char *)malloc(s.len + 1);
    if (!r) return NULL;
    if (s.len) memcpy(r, s.ptr, s.len);
    r[s.len] = '\0';
    return r;
}

static int contains(MdStr haystack, const char *needle) {
    size_t n = strlen(needle);
    if (n > haystack.len) return 0;
    for (size_t i = 0; i + n <= haystack.len; i++) {
        if (memcmp(haystack.ptr + i, needle, n) == 0) return 1;
    }
    return 0;
}

/* ---- blob store ------------------------------------------------------------------------- */

typedef struct Blob {
    char *key;
    uint8_t *data;
    size_t len;
    struct Blob *next;
} Blob;

typedef struct Store {
    md_lock lock;
    Blob *head;
} Store;

/* Returns the link pointing to the blob with `key`, or to the terminating NULL. Lock held. */
static Blob **find(Store *s, const char *key) {
    Blob **link = &s->head;
    while (*link && strcmp((*link)->key, key) != 0) link = &(*link)->next;
    return link;
}

/* put: the data arrives as a stream; each completed read issues the next one. */
typedef struct PutOp {
    Store *store;
    char *key;
    uint8_t *data;
    size_t len, cap;
    MdReadStream stream;
    MdCompletion done;
} PutOp;

static void put_read_next(PutOp *op);

static void put_finish(PutOp *op, MdStatus status, MdBuffer result) {
    op->stream.vtable->release(op->stream.instance);
    complete(op->done, status, result);
    free(op->key);
    free(op->data);
    free(op);
}

static void put_on_chunk(void *ctx, MdStatus status, MdBuffer chunk) {
    PutOp *op = (PutOp *)ctx;
    if (status != MD_OK) {
        put_finish(op, status, chunk); /* forward the producer's error message */
        return;
    }
    if (chunk.len == 0) { /* end of stream: store the blob */
        buffer_free(&chunk);
        char json[64];
        snprintf(json, sizeof json, "{\"size\":%zu}", op->len);
        lock_acquire(&op->store->lock);
        Blob **link = find(op->store, op->key);
        Blob *b = *link;
        if (!b) {
            b = (Blob *)calloc(1, sizeof *b);
            if (!b) {
                lock_release(&op->store->lock);
                put_finish(op, MD_RESOURCE_EXHAUSTED, buffer_str("out of memory"));
                return;
            }
            b->key = op->key;
            op->key = NULL;
            *link = b;
        } else {
            free(b->data);
        }
        b->data = op->data;
        b->len = op->len;
        op->data = NULL;
        lock_release(&op->store->lock);
        put_finish(op, MD_OK, buffer_str(json));
        return;
    }
    if (op->len + chunk.len > op->cap) {
        size_t cap = op->cap ? op->cap : CHUNK_SIZE;
        while (cap < op->len + chunk.len) cap *= 2;
        uint8_t *grown = (uint8_t *)realloc(op->data, cap);
        if (!grown) {
            buffer_free(&chunk);
            put_finish(op, MD_RESOURCE_EXHAUSTED, buffer_str("out of memory"));
            return;
        }
        op->data = grown;
        op->cap = cap;
    }
    memcpy(op->data + op->len, chunk.ptr, chunk.len);
    op->len += chunk.len;
    buffer_free(&chunk);
    put_read_next(op);
}

static void put_read_next(PutOp *op) {
    MdCompletion c = {op, put_on_chunk};
    op->stream.vtable->read(op->stream.instance, c);
}

static void blob_put(void *instance, MdStr key, MdReadStream data, MdCancel cancel,
                     MdCompletion done) {
    (void)cancel;
    PutOp *op = (PutOp *)calloc(1, sizeof *op);
    char *k = dup_str(key);
    if (!op || !k) {
        free(op);
        free(k);
        data.vtable->release(data.instance);
        complete_msg(done, MD_RESOURCE_EXHAUSTED, "out of memory");
        return;
    }
    op->store = (Store *)instance;
    op->key = k;
    op->stream = data;
    op->done = done;
    put_read_next(op);
}

/* get: returns a stream over a private copy of the requested bytes. */
typedef struct GetStream {
    uint8_t *data;
    size_t len, pos;
} GetStream;

static void get_stream_read(void *instance, MdCompletion done) {
    GetStream *gs = (GetStream *)instance;
    size_t n = gs->len - gs->pos;
    if (n > CHUNK_SIZE) n = CHUNK_SIZE;
    MdBuffer chunk = buffer_copy(gs->data + gs->pos, n);
    if (n > 0 && chunk.len == 0) {
        complete_msg(done, MD_RESOURCE_EXHAUSTED, "out of memory");
        return;
    }
    gs->pos += n;
    complete(done, MD_OK, chunk); /* empty chunk = end of stream */
}

static void get_stream_release(void *instance) {
    GetStream *gs = (GetStream *)instance;
    free(gs->data);
    free(gs);
}

static const MdReadStreamVtable GET_STREAM_VTABLE = {
    sizeof(MdReadStreamVtable),
    get_stream_read,
    get_stream_release,
};

static void stream_fail(MdStreamCompletion done, MdStatus status, const char *msg) {
    MdReadStream none = {NULL, NULL};
    done.complete(done.ctx, status, none, buffer_str(msg));
}

static void blob_get(void *instance, MdStr key, MdRange range, MdCancel cancel,
                     MdStreamCompletion done) {
    (void)cancel;
    Store *s = (Store *)instance;
    char *k = dup_str(key);
    if (!k) {
        stream_fail(done, MD_RESOURCE_EXHAUSTED, "out of memory");
        return;
    }
    lock_acquire(&s->lock);
    Blob *b = *find(s, k);
    free(k);
    if (!b) {
        lock_release(&s->lock);
        stream_fail(done, MD_NOT_FOUND, "no such blob");
        return;
    }
    uint64_t start = 0, end = b->len;
    if (range.has_range) {
        start = range.start;
        end = range.end;
    }
    if (start > end || end > b->len) {
        lock_release(&s->lock);
        stream_fail(done, MD_INVALID_ARGUMENT, "range outside blob");
        return;
    }
    GetStream *gs = (GetStream *)calloc(1, sizeof *gs);
    size_t n = (size_t)(end - start);
    uint8_t *copy = n ? (uint8_t *)malloc(n) : NULL;
    if (!gs || (n && !copy)) {
        lock_release(&s->lock);
        free(gs);
        free(copy);
        stream_fail(done, MD_RESOURCE_EXHAUSTED, "out of memory");
        return;
    }
    if (n) memcpy(copy, b->data + start, n);
    lock_release(&s->lock);
    gs->data = copy;
    gs->len = n;
    MdReadStream stream = {gs, &GET_STREAM_VTABLE};
    done.complete(done.ctx, MD_OK, stream, buffer_empty());
}

static void blob_stat(void *instance, MdStr key, MdCancel cancel, MdCompletion done) {
    (void)cancel;
    Store *s = (Store *)instance;
    char *k = dup_str(key);
    if (!k) {
        complete_msg(done, MD_RESOURCE_EXHAUSTED, "out of memory");
        return;
    }
    char json[64] = "null";
    lock_acquire(&s->lock);
    Blob *b = *find(s, k);
    if (b) snprintf(json, sizeof json, "{\"size\":%zu}", b->len);
    lock_release(&s->lock);
    free(k);
    complete(done, MD_OK, buffer_str(json));
}

static void blob_remove(void *instance, MdStr key, MdCancel cancel, MdCompletion done) {
    (void)cancel;
    Store *s = (Store *)instance;
    char *k = dup_str(key);
    if (!k) {
        complete_msg(done, MD_RESOURCE_EXHAUSTED, "out of memory");
        return;
    }
    lock_acquire(&s->lock);
    Blob **link = find(s, k);
    Blob *b = *link;
    if (b) *link = b->next;
    lock_release(&s->lock);
    free(k);
    if (b) {
        free(b->key);
        free(b->data);
        free(b);
    }
    complete(done, MD_OK, buffer_empty()); /* deleting a missing blob is not an error */
}

static void blob_usage(void *instance, MdCancel cancel, MdCompletion done) {
    (void)instance;
    (void)cancel;
    complete(done, MD_OK, buffer_str("null")); /* no physical limit */
}

static void blob_destroy(void *instance) {
    Store *s = (Store *)instance;
    for (Blob *b = s->head; b;) {
        Blob *next = b->next;
        free(b->key);
        free(b->data);
        free(b);
        b = next;
    }
    lock_destroy(&s->lock);
    free(s);
}

static const MdBlobStoreVtable BLOB_STORE_VTABLE = {
    sizeof(MdBlobStoreVtable),
    blob_put,
    blob_get,
    blob_stat,
    blob_remove,
    blob_usage,
    blob_destroy,
};

/* ---- event hook ----------------------------------------------------------------------------- */

typedef struct Hook {
    md_lock lock;
    unsigned long long count;
} Hook;

static MdBuffer hook_filter(void *instance) {
    (void)instance;
    return buffer_str("{\"kinds\":[]}"); /* all events */
}

static void hook_on_event(void *instance, MdStr event, MdCancel cancel, MdCompletion done) {
    (void)cancel;
    Hook *h = (Hook *)instance;
    if (contains(event, "\"kind\":\"test.fail\"")) {
        complete_msg(done, MD_UNAVAILABLE, "refusing test.fail");
        return;
    }
    lock_acquire(&h->lock);
    h->count++;
    lock_release(&h->lock);
    complete(done, MD_OK, buffer_empty());
}

static void hook_destroy(void *instance) {
    Hook *h = (Hook *)instance;
    lock_destroy(&h->lock);
    free(h);
}

static const MdEventHookVtable EVENT_HOOK_VTABLE = {
    sizeof(MdEventHookVtable),
    hook_filter,
    hook_on_event,
    hook_destroy,
};

/* ---- entry points --------------------------------------------------------------------------- */

MEDICUS_ADDON_EXPORT uint32_t medicus_addon_abi_version(void) { return MD_ABI_VERSION; }

/* The manifest: read by the host before init, so it is a constant. It declares the two capability
 * kinds this addon registers; registering any other kind would be refused. */
static const char MANIFEST[] =
    "{\"name\":\"example-c\",\"version\":\"0.1.0\",\"target\":\"" MD_ARCH "-" MD_OS "\","
    "\"provides\":[\"blob_store\",\"event_hook\"]}";

MEDICUS_ADDON_EXPORT MdStr medicus_addon_manifest(void) {
    MdStr m = {(const uint8_t *)MANIFEST, sizeof MANIFEST - 1};
    return m;
}

MEDICUS_ADDON_EXPORT MdStatus medicus_addon_init(const MdHostApi *host) {
    if (!host || host->size < sizeof(MdHostApi) || host->abi_version != MD_ABI_VERSION) {
        return MD_UNSUPPORTED;
    }
    g_host = host;

    Store *store = (Store *)calloc(1, sizeof *store);
    Hook *hook = (Hook *)calloc(1, sizeof *hook);
    if (!store || !hook) {
        free(store);
        free(hook);
        return MD_RESOURCE_EXHAUSTED;
    }
    lock_init(&store->lock);
    lock_init(&hook->lock);

    /* On refusal the host has not taken ownership, so we free the instance ourselves. */
    MdStatus status = host->register_capability(host->ctx, MD_CAP_BLOB_STORE, str("memory"),
                                                &BLOB_STORE_VTABLE, store);
    if (status != MD_OK) {
        blob_destroy(store);
        hook_destroy(hook);
        return status;
    }
    status = host->register_capability(host->ctx, MD_CAP_EVENT_HOOK, str("count-events"),
                                       &EVENT_HOOK_VTABLE, hook);
    if (status != MD_OK) {
        hook_destroy(hook);
        return status;
    }

    log_msg(MD_LOG_INFO, "example-c initialized");
    return MD_OK;
}
