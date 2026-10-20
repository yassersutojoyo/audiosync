/*
 * The panic probe (CI, docs/ENGINE_SPEC.md S205): proves that a panic inside
 * an asc_airplay_* call still stops at the C boundary in the library the app
 * links, prelinked the way scripts/build_core.sh prelinks it.
 *
 * AUDIOSYNC_CORE_PROBE=1 scripts/build_core.sh builds that library with one
 * extra entry, asc_airplay_panic_probe, which panics inside the guard every
 * sender call runs its work behind. This program makes that call and
 * expects the call's failure value, then the sender's failure event, whose
 * detail names where the panic happened and not what it said, and then a
 * normal exit with status 0. A panic that got past the guard would abort
 * the process instead.
 *
 * Build and run (macOS, from the repository root):
 *   AUDIOSYNC_CORE_PROBE=1 scripts/build_core.sh
 *   cc -std=c11 -Wall -Wextra -I core/include Tests/panic-probe.c \
 *     core/target/universal-apple-darwin/probe/libaudiosync_core.a -o panic-probe
 *   ./panic-probe
 */

#include <stdbool.h>
#include <stdio.h>
#include <string.h>

#include "audiosync_core.h"

/* Only in the probe's build of the library, so not in the header. */
bool asc_airplay_panic_probe(AscAirPlay *airplay);

static int failed(const char *what) {
    fprintf(stderr, "panic probe: FAILED: %s\n", what);
    return 1;
}

int main(void) {
    static const char where[] = "panic at src/panic_probe.rs:";

    AscAirPlay *airplay = asc_airplay_create();
    if (airplay == NULL) {
        return failed("asc_airplay_create returned NULL");
    }
    if (asc_airplay_panic_probe(airplay)) {
        return failed("the probe returned true: the call ran without its panic");
    }

    AscAirPlayEvent event;
    memset(&event, 0, sizeof event);
    if (!asc_airplay_next_event(airplay, &event)) {
        return failed("no event after the panic");
    }
    if (event.kind != ASC_AIRPLAY_EVENT_FAILED) {
        fprintf(stderr, "panic probe: event kind %d\n", (int)event.kind);
        return failed("the first event after the panic is not the failure");
    }
    const char *detail = event.detail != NULL ? event.detail : "(null)";
    printf("panic probe: failure detail \"%s\"\n", detail);
    if (strncmp(detail, where, strlen(where)) != 0) {
        return failed("the failure's detail does not say where the panic happened");
    }
    if (strstr(detail, "own message") != NULL) {
        return failed("the failure's detail holds the panic's message");
    }

    if (asc_airplay_panic_probe(airplay)) {
        return failed("a dead sender's call returned true");
    }
    if (asc_airplay_next_event(airplay, &event)) {
        return failed("an event after the failure");
    }
    asc_airplay_destroy(&airplay);
    if (airplay != NULL) {
        return failed("asc_airplay_destroy left the handle pointer set");
    }

    printf("panic probe: PASSED (the panic stopped at the boundary)\n");
    return 0;
}
