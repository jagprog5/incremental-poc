# Incremental Agent

Suppose a file system is being scanned on some interval. Since the scan is
frequent, it's more efficient to only scan the files that have changed since the
previous scan.

This can be implemented using a "last scan timestamp", but that still requires
checking the modify timestamp of each file in the file system. Additionally, a
"last scan timestamp" does not keep track of file deletions (suppose a file with
a security findings has been removed, this will not be detected).

This provides an agent which tracks file changes on a system since a previous
scan and provides it to the scanner on request.

# Considerations

## Memory

There is a upper bound on memory usage; the number of files to track has a
configurable limit. If that limit is exceeded, then it drops all file changes
stored. When the scanner requests what files have changed, it will receive a
response indicating that too many files have changed, at which point it should
fall back to a full file system scan.

## Feedback Loop

It is the responsibility of both the scanner and the incremental agent to ignore
file changes which they themselves create. For example, if the scanner is
concurrently creating or adding files while paging through the incremental
agent's change list then this could cause a loop (since it is causing its own
new files to scan). This is only a problem if the scanner itself creates one or
more files per file being scanned (highly unlikely).

At a worst case scenario, this would eventually exit once the configurable
memory limit is exceeded, as mentioned above.
