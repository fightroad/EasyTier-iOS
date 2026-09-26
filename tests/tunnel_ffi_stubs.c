#include "easytier_ios.h"
#include <pthread.h>
#include <stdlib.h>
#include <string.h>

static pthread_mutex_t lock = PTHREAD_MUTEX_INITIALIZER;
static char *status_json;
static int ack_count;
static uint64_t ack_generation;
static bool ack_success;
static int web_start_count;
static bool web_secure_mode;

void test_set_status(const char *json) {
    pthread_mutex_lock(&lock);
    free(status_json);
    status_json = strdup(json);
    pthread_mutex_unlock(&lock);
}
int test_ack_count(void) { return ack_count; }
uint64_t test_ack_generation(void) { return ack_generation; }
bool test_ack_success(void) { return ack_success; }
int test_web_start_count(void) { return web_start_count; }
bool test_web_secure_mode(void) { return web_secure_mode; }

int32_t get_instance_status(const char **json, const char **err) {
    pthread_mutex_lock(&lock);
    *json = strdup(status_json);
    pthread_mutex_unlock(&lock);
    return 0;
}
int32_t get_running_info(const char **json, const char **err) {
    *json = strdup("{\"routes\":[]}");
    return 0;
}
int32_t complete_instance_setup(uint64_t generation, bool success, const char *error) {
    ack_count++;
    ack_generation = generation;
    ack_success = success;
    return 0;
}
void free_string(const char *s) { free((void *)s); }
int32_t set_instance_tun_fd(uint64_t generation, int32_t fd, const char **err) { abort(); }
int32_t init_logger(const char *p, const char *l, const char *s, uint64_t max_bytes, int32_t enable_file_log, const char **e) { return 0; }
int32_t clear_logger(const char **e) { return 0; }
int32_t run_network_instance(const char *c, instance_event_callback_t cb, const char **e) { return 0; }
int32_t start_config_server_client(const char *u, const char *h, const char *m, bool secure_mode, instance_event_callback_t c, const char **e) {
    web_start_count++;
    web_secure_mode = secure_mode;
    return 0;
}
int32_t is_config_server_client_connected(void) { return 1; }
int32_t stop_network_instance(void) { return 0; }
