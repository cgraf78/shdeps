#define _GNU_SOURCE

#include <errno.h>
#include <fcntl.h>
#include <limits.h>
#include <signal.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/syscall.h>
#include <unistd.h>

#ifndef SYS_pidfd_open
#define SYS_pidfd_open 434
#endif

#ifndef SYS_pidfd_send_signal
#define SYS_pidfd_send_signal 424
#endif

static int parse_unsigned(const char* text, unsigned long long maximum, unsigned long long* value) {
  char* end = NULL;
  errno = 0;
  unsigned long long parsed = strtoull(text, &end, 10);
  if (errno != 0 || end == text || *end != '\0' || parsed == 0 || parsed > maximum) {
    return -1;
  }
  *value = parsed;
  return 0;
}

static int read_start_time(pid_t pid, unsigned long long* start_time) {
  char path[64];
  if (snprintf(path, sizeof(path), "/proc/%d/stat", pid) >= (int)sizeof(path)) {
    return -1;
  }
  int fd = open(path, O_RDONLY | O_CLOEXEC | O_NOFOLLOW);
  if (fd < 0) {
    return -1;
  }
  char buffer[8192];
  ssize_t length = read(fd, buffer, sizeof(buffer) - 1);
  int saved_errno = errno;
  close(fd);
  if (length <= 0) {
    errno = saved_errno;
    return -1;
  }
  buffer[length] = '\0';

  char* close_paren = strrchr(buffer, ')');
  if (close_paren == NULL || close_paren[1] != ' ') {
    return -1;
  }
  char* save = NULL;
  char* field = strtok_r(close_paren + 2, " \t\r\n", &save);
  for (int index = 0; index < 19 && field != NULL; ++index) {
    field = strtok_r(NULL, " \t\r\n", &save);
  }
  if (field == NULL) {
    return -1;
  }
  return parse_unsigned(field, ULLONG_MAX, start_time);
}

int main(int argc, char** argv) {
  if (argc != 3) {
    fprintf(stderr, "usage: %s PID:START SIGNAL\n", argv[0]);
    return 2;
  }
  char* separator = strchr(argv[1], ':');
  if (separator == NULL || separator == argv[1] || separator[1] == '\0') {
    return 2;
  }
  *separator = '\0';
  unsigned long long parsed_pid = 0;
  unsigned long long expected_start = 0;
  unsigned long long parsed_signal = 0;
  if (parse_unsigned(argv[1], INT_MAX, &parsed_pid) != 0 ||
      parse_unsigned(separator + 1, ULLONG_MAX, &expected_start) != 0) {
    return 2;
  }
  char* signal_end = NULL;
  errno = 0;
  parsed_signal = strtoull(argv[2], &signal_end, 10);
  if (errno != 0 || signal_end == argv[2] || *signal_end != '\0' ||
      parsed_signal > (unsigned long long)SIGRTMAX) {
    return 2;
  }

  int pidfd = (int)syscall(SYS_pidfd_open, (pid_t)parsed_pid, 0);
  if (pidfd < 0) {
    return errno == ESRCH || errno == ENOENT ? 3 : 1;
  }
  unsigned long long current_start = 0;
  if (read_start_time((pid_t)parsed_pid, &current_start) != 0 || current_start != expected_start) {
    close(pidfd);
    return 3;
  }
  int result = (int)syscall(SYS_pidfd_send_signal, pidfd, (int)parsed_signal, NULL, 0);
  int saved_errno = errno;
  close(pidfd);
  if (result == 0) {
    return 0;
  }
  errno = saved_errno;
  return errno == ESRCH ? 3 : 1;
}
