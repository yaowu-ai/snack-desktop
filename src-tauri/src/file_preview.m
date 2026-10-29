#import <AppKit/AppKit.h>
#import <Quartz/Quartz.h>

@interface SnackFilePreviewOwner : NSObject <QLPreviewPanelDataSource, QLPreviewPanelDelegate, NSWindowDelegate>
@property(nonatomic, strong) NSURL *file;
@property(nonatomic, strong) NSTimer *visibilityTimer;
- (void)clearFile;
@end

@implementation SnackFilePreviewOwner
- (NSInteger)numberOfPreviewItemsInPreviewPanel:(QLPreviewPanel *)panel { return self.file ? 1 : 0; }
- (id<QLPreviewItem>)previewPanel:(QLPreviewPanel *)panel previewItemAtIndex:(NSInteger)index { return self.file; }
- (void)windowWillClose:(NSNotification *)notification {
  [self clearFile];
}
- (void)clearFile {
  [self.visibilityTimer invalidate];
  self.visibilityTimer = nil;
  if (self.file) [[NSFileManager defaultManager] removeItemAtURL:[self.file URLByDeletingLastPathComponent] error:nil];
  self.file = nil;
}
@end

static SnackFilePreviewOwner *activeOwner;

// Called on the AppKit main thread. The panel owns access until it closes.
bool snack_preview_file(const char *path) {
  if (!activeOwner) activeOwner = [SnackFilePreviewOwner new];
  SnackFilePreviewOwner *owner = activeOwner;
  QLPreviewPanel *panel = [QLPreviewPanel sharedPreviewPanel];
  if (!panel) return false;
  [owner clearFile];
  owner.file = [NSURL fileURLWithPath:[NSString stringWithUTF8String:path]];
  panel.dataSource = owner;
  panel.delegate = owner;
  [panel reloadData];
  [panel makeKeyAndOrderFront:nil];
  // Quick Look can hide its shared panel without sending windowWillClose.
  owner.visibilityTimer = [NSTimer scheduledTimerWithTimeInterval:0.25 repeats:YES block:^(NSTimer *timer) {
    if (!panel.isVisible) [owner clearFile];
  }];
  [NSApp activateIgnoringOtherApps:YES];
  return true;
}

// A stale session watcher must never close a newer preview.
void snack_close_file_preview(const char *path) {
  if (![activeOwner.file.path isEqualToString:[NSString stringWithUTF8String:path]]) return;
  [[QLPreviewPanel sharedPreviewPanel] orderOut:nil];
  [activeOwner clearFile];
}
