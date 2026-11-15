


volatile int locations_library_sink;


__attribute__((visibility("default"), noinline)) int locations_in_library(int value,
                                                                          void (*stop)(void)) {
    locations_library_sink = value;
    stop();
    return locations_library_sink;
}


__attribute__((visibility("default"), noinline)) int
locations_interposed_target(int value, void (*stop)(void)) {
    locations_library_sink = value;
    stop();
    return locations_library_sink;
}


__attribute__((visibility("default"), noinline)) int locations_interposed(int value,
                                                                          void (*stop)(void)) {
    return locations_interposed_target(value + 1, stop);
}



__attribute__((visibility("default"), noinline)) int locations_interposing(int value,
                                                                           void (*stop)(void)) {
    return locations_interposed(value * 2, stop);
}
