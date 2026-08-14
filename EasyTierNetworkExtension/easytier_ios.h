#ifndef EASYTIER_IOS_H
#define EASYTIER_IOS_H

#include <stddef.h>
#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

/// max_bytes == 0 means use the core default size cap.
/// enable_file_log == 0 disables easytier.log (OSLog only).
int32_t init_logger(const char *path, const char *level, const char *subsystem, uint64_t max_bytes, int32_t enable_file_log, const char **err_msg);

int32_t clear_logger(const char **err_msg);

int32_t set_tun_fd(int32_t fd, const char **err_msg);

void free_string(const char *s);

int32_t run_network_instance(const char *cfg_str, const char **err_msg);

int32_t stop_network_instance(void);

int32_t register_stop_callback(void (*callback)(void), const char **err_msg);

int32_t register_running_info_callback(void (*callback)(void), const char **err_msg);

int32_t get_running_info(const char **json, const char **err_msg);

int32_t get_latest_error_msg(const char **msg, const char **err_msg);

#ifdef __cplusplus
}
#endif

#endif
