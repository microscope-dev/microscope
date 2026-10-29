// Node-API bridge from index.js to the probes SDK's run-time sites
// (sdk/rust-probes/src/c.rs).
//
// The library is found at MICROSCOPE_PROBES_LIBRARY, or else next to this
// addon. Without it, or with one that has no run-time sites, the addon
// still loads: its `problem` says why, and every call does nothing.
//
// Sites are program-lifetime pointers passed to JavaScript as BigInts;
// 0n means no site. The context-aware addon shares one library across workers.

#define _GNU_SOURCE
#include <dlfcn.h>
#include <limits.h>
#include <pthread.h>
#include <stdbool.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#include <node_api.h>

// The run-time site ABI this addon calls.
#define ABI 2

typedef const void *site_t;

static struct {
	uint16_t (*abi)(void);
	bool (*listening)(void);
	bool (*found)(void);
	site_t (*define)(uint8_t, const char *, size_t, const char *, size_t, uint32_t);
	bool (*on)(site_t);
	bool (*zone_begin)(site_t);
	void (*zone_end)(site_t);
	bool (*label_begin)(site_t, const char *, size_t);
	void (*label_end)(site_t);
	void (*event)(site_t);
	void (*plot)(site_t, double);
	void (*message)(site_t, const char *, size_t);
} library;

// Why the library cannot be used, or empty.
static char problem[512];
static pthread_once_t loaded = PTHREAD_ONCE_INIT;

// The library next to the file that holds `symbol`, into `path`.
static void beside(const void *symbol, char *path, size_t size)
{
	Dl_info info;
	path[0] = 0;
	if (!dladdr(symbol, &info) || !info.dli_fname)
		return;
	const char *slash = strrchr(info.dli_fname, '/');
	int directory = slash ? (int)(slash - info.dli_fname) : 1;
	const char *base = slash ? info.dli_fname : ".";
	snprintf(path, size, "%.*s/libmicroscope_probes.so", directory, base);
}

static void load(void)
{
	char found[PATH_MAX];
	const char *path = getenv("MICROSCOPE_PROBES_LIBRARY");
	if (!path || !*path) {
		beside((const void *)load, found, sizeof(found));
		path = found;
	}
	void *handle = dlopen(path, RTLD_NOW | RTLD_LOCAL);
	if (!handle) {
		snprintf(problem, sizeof(problem), "the probes library could not be loaded: %s",
			 dlerror());
		return;
	}
	struct {
		const char *name;
		void **slot;
	} symbols[] = {
		{"microscope_probes_abi", (void **)&library.abi},
		{"microscope_probes_listening", (void **)&library.listening},
		{"microscope_probes_found", (void **)&library.found},
		{"microscope_probes_define", (void **)&library.define},
		{"microscope_probes_on", (void **)&library.on},
		{"microscope_probes_zone_begin", (void **)&library.zone_begin},
		{"microscope_probes_zone_end", (void **)&library.zone_end},
		{"microscope_probes_label_begin", (void **)&library.label_begin},
		{"microscope_probes_label_end", (void **)&library.label_end},
		{"microscope_probes_event", (void **)&library.event},
		{"microscope_probes_plot", (void **)&library.plot},
		{"microscope_probes_message", (void **)&library.message},
	};
	for (size_t i = 0; i < sizeof(symbols) / sizeof(symbols[0]); i++) {
		*symbols[i].slot = dlsym(handle, symbols[i].name);
		if (!*symbols[i].slot) {
			snprintf(problem, sizeof(problem),
				 "the probes library at %s is not one this package uses: it has no %s",
				 path, symbols[i].name);
			memset(&library, 0, sizeof(library));
			return;
		}
	}
	if (library.abi() < ABI) {
		snprintf(problem, sizeof(problem),
			 "the probes library at %s has no run-time sites", path);
		memset(&library, 0, sizeof(library));
	}
}

// The site an argument passes, or NULL.
static site_t site_of(napi_env env, napi_value value)
{
	uint64_t address = 0;
	bool lossless = false;
	if (napi_get_value_bigint_uint64(env, value, &address, &lossless) != napi_ok || !lossless)
		return NULL;
	return (site_t)(uintptr_t)address;
}

// The UTF-8 bytes of a string argument, which the caller frees, and their
// length; NULL where it is none.
static char *text_of(napi_env env, napi_value value, size_t *length)
{
	if (napi_get_value_string_utf8(env, value, NULL, 0, length) != napi_ok)
		return NULL;
	char *text = malloc(*length + 1);
	if (text && napi_get_value_string_utf8(env, value, text, *length + 1, length) != napi_ok) {
		free(text);
		return NULL;
	}
	return text;
}

// The arguments of a call, up to `wanted`.
static size_t arguments(napi_env env, napi_callback_info info, napi_value *values, size_t wanted)
{
	size_t count = wanted;
	if (napi_get_cb_info(env, info, &count, values, NULL, NULL) != napi_ok)
		return 0;
	return count < wanted ? count : wanted;
}

static napi_value boolean(napi_env env, bool value)
{
	napi_value result = NULL;
	napi_get_boolean(env, value, &result);
	return result;
}

static napi_value nothing(napi_env env)
{
	napi_value result = NULL;
	napi_get_undefined(env, &result);
	return result;
}

static napi_value listening(napi_env env, napi_callback_info info)
{
	(void)info;
	return boolean(env, library.listening && library.listening());
}

static napi_value found(napi_env env, napi_callback_info info)
{
	(void)info;
	return boolean(env, library.found && library.found());
}

// define(kind, name, file, line): the site, or 0n.
static napi_value define(napi_env env, napi_callback_info info)
{
	napi_value values[4];
	uint64_t address = 0;
	uint32_t kind = 0, line = 0;
	size_t name_length = 0, file_length = 0;
	char *name = NULL, *file = NULL;
	if (library.define && arguments(env, info, values, 4) == 4 &&
	    napi_get_value_uint32(env, values[0], &kind) == napi_ok && kind <= UINT8_MAX &&
	    napi_get_value_uint32(env, values[3], &line) == napi_ok &&
	    (name = text_of(env, values[1], &name_length)) &&
	    (file = text_of(env, values[2], &file_length)))
		address = (uint64_t)(uintptr_t)library.define((uint8_t)kind, name, name_length, file,
							      file_length, line);
	free(name);
	free(file);
	napi_value result = NULL;
	napi_create_bigint_uint64(env, address, &result);
	return result;
}

// A call of the library on the site its first argument passes.
#define SITE_CALL(function, call)                                                     \
	static napi_value function(napi_env env, napi_callback_info info)             \
	{                                                                             \
		napi_value values[1];                                                 \
		site_t site = arguments(env, info, values, 1) == 1 ? site_of(env, values[0]) \
								   : NULL;           \
		call;                                                                 \
	}

SITE_CALL(on, return boolean(env, site && library.on && library.on(site)))
SITE_CALL(zone_begin, return boolean(env, site && library.zone_begin && library.zone_begin(site)))
SITE_CALL(zone_end, if (site && library.zone_end) library.zone_end(site); return nothing(env))
SITE_CALL(label_end, if (site && library.label_end) library.label_end(site); return nothing(env))
SITE_CALL(event, if (site && library.event) library.event(site); return nothing(env))

// labelBegin(site, text): whether it began, and so must end.
static napi_value label_begin(napi_env env, napi_callback_info info)
{
	napi_value values[2];
	bool began = false;
	if (library.label_begin && arguments(env, info, values, 2) == 2) {
		site_t site = site_of(env, values[0]);
		size_t length = 0;
		char *text = site ? text_of(env, values[1], &length) : NULL;
		if (text)
			began = library.label_begin(site, text, length);
		free(text);
	}
	return boolean(env, began);
}

// plot(site, value)
static napi_value plot(napi_env env, napi_callback_info info)
{
	napi_value values[2];
	double value = 0;
	if (library.plot && arguments(env, info, values, 2) == 2 &&
	    napi_get_value_double(env, values[1], &value) == napi_ok) {
		site_t site = site_of(env, values[0]);
		if (site)
			library.plot(site, value);
	}
	return nothing(env);
}

// message(site, text)
static napi_value message(napi_env env, napi_callback_info info)
{
	napi_value values[2];
	if (library.message && arguments(env, info, values, 2) == 2) {
		site_t site = site_of(env, values[0]);
		size_t length = 0;
		char *text = site ? text_of(env, values[1], &length) : NULL;
		if (text)
			library.message(site, text, length);
		free(text);
	}
	return nothing(env);
}

NAPI_MODULE_INIT(/* napi_env env, napi_value exports */)
{
	pthread_once(&loaded, load);
	napi_property_descriptor properties[] = {
		{"listening", NULL, listening, NULL, NULL, NULL, napi_default, NULL},
		{"found", NULL, found, NULL, NULL, NULL, napi_default, NULL},
		{"define", NULL, define, NULL, NULL, NULL, napi_default, NULL},
		{"on", NULL, on, NULL, NULL, NULL, napi_default, NULL},
		{"zoneBegin", NULL, zone_begin, NULL, NULL, NULL, napi_default, NULL},
		{"zoneEnd", NULL, zone_end, NULL, NULL, NULL, napi_default, NULL},
		{"labelBegin", NULL, label_begin, NULL, NULL, NULL, napi_default, NULL},
		{"labelEnd", NULL, label_end, NULL, NULL, NULL, napi_default, NULL},
		{"event", NULL, event, NULL, NULL, NULL, napi_default, NULL},
		{"plot", NULL, plot, NULL, NULL, NULL, napi_default, NULL},
		{"message", NULL, message, NULL, NULL, NULL, napi_default, NULL},
	};
	if (napi_define_properties(env, exports, sizeof(properties) / sizeof(properties[0]),
				   properties) != napi_ok)
		return NULL;
	napi_value why = NULL;
	if (problem[0])
		napi_create_string_utf8(env, problem, NAPI_AUTO_LENGTH, &why);
	else
		napi_get_null(env, &why);
	napi_set_named_property(env, exports, "problem", why);
	return exports;
}
