#include "lan_history.h"

#include <assert.h>
#include <stdio.h>
#include <string.h>

int main(void)
{
    LanHistory history = {0}, loaded = {0};
    char encoded[LAN_HISTORY_TEXT_MAX];

    assert(!lan_history_parse(NULL, &loaded));
    assert(!lan_history_parse("v2|bad", &loaded));
    assert(lan_history_record(&history, "0123456789abcdef", 1, 3, 1, 1234));
    assert(!lan_history_record(&history, "0123456789abcdef", 1, 3, 1, 1234));
    assert(lan_history_record(&history, "0123456789abcdef", 2, 3, 2, 1400));
    assert(lan_history_record(&history, "0123456789abcdef", 3, 3, 0, -1)); /* DNF */
    assert(lan_history_record(&history, "0123456789abcdef", 4, 3, 10, 1500));
    assert(history.track[3].races == 4);
    assert(history.track[3].place[0] == 1);
    assert(history.track[3].place[1] == 1);
    assert(history.track[3].place[9] == 1);
    assert(history.track[3].best_ticks == 1234);
    assert(!lan_history_record(&history, "0123456789abcdef", 2, 3, 2, 1400)); /* older snapshot */
    assert(lan_history_record(&history, "ROOM123", 1, 4, 1, 2000)); /* new room */

    assert(lan_history_format(&history, encoded, sizeof encoded));
    assert(lan_history_parse(encoded, &loaded));
    assert(!strcmp(loaded.room_id, "ROOM123"));
    assert(loaded.last_race_id == 1);
    assert(loaded.track[3].races == 4);
    assert(loaded.track[3].place[0] == 1);
    assert(loaded.track[3].place[1] == 1);
    assert(loaded.track[3].place[9] == 1);
    assert(loaded.track[3].best_ticks == 1234);
    assert(loaded.track[4].races == 1);
    assert(loaded.track[4].best_ticks == 2000);

    encoded[strlen(encoded) - 1] = 'x';
    assert(!lan_history_parse(encoded, &loaded));
    puts("LAN history tests passed");
    return 0;
}
