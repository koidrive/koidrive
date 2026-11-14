# Conflicts

Each change that a device sends reaches the server in some order. When two devices change the same file and neither had seen the other's change, the server keeps both. Koidrive never picks one edit and drops the other.

## Two edits of one file

The edit that reaches the server first becomes the file's new content. The second edit goes into a new file next to it, named after the device that made it and the date in UTC:

```
report.txt
report (conflict from laptop 2026-11-10).txt
```

Both files sync to every device. Compare them, keep what you need, and delete the copy. The device whose edit became the copy also opens a [decision](status-reference.md#a-conflict-opens-a-decision) that keeps either file or both for you. On a Mac, the decision appears in the app's [Needs You](macos.md#check-sync-and-answer-questions) window. If that name is taken too, Koidrive adds a number before the closing parenthesis: `report (conflict from laptop 2026-11-10 2).txt`. Koidrive shortens a long device name, and then the file name, so the result fits in 255 bytes.

An edit and a rename of the same file don't conflict. The file keeps the new content under the new name.

## An edit and a delete

When one device deletes a file that another device edited in the meantime, the file stays, with the edit. A deleted folder keeps every file that another device changed or created in it meanwhile, and the folders above those files. The rest of the folder goes to the trash.

## Two files with the same name

When two devices create a file with the same name in the same folder, or move files to the same name, the file that reaches the server second takes a conflict name like the one above. Names compare without regard to case, so `Report.txt` and `report.txt` count as the same name.

Two folders created with the same name become one folder that holds the contents of both.

## Moves

Two moves of the same file or folder don't conflict. The move that reaches the server last wins. A move that would put a folder inside itself, for example when two devices move two folders into each other at the same time, is refused, and the device that made it moves the folder back.
