#import <AVFoundation/AVFoundation.h>
#import <CoreGraphics/CoreGraphics.h>
#import <ScreenCaptureKit/ScreenCaptureKit.h>

@interface SnackRecordingBridge : NSObject <SCStreamOutput, SCStreamDelegate>
@property(nonatomic, strong) AVAudioEngine *engine;
@property(nonatomic, strong) AVAudioFile *microphoneFile;
@property(nonatomic, strong) AVAssetWriter *systemWriter;
@property(nonatomic, strong) AVAssetWriterInput *systemInput;
@property(nonatomic, strong) SCStream *stream;
@property(nonatomic, strong) NSURL *microphoneURL;
@property(nonatomic, strong) NSURL *systemURL;
@property(nonatomic, copy) NSString *outputBase;
@property(nonatomic) dispatch_queue_t audioQueue;
@property(nonatomic) BOOL writerStarted;
@property(nonatomic) BOOL microphoneTapInstalled;
@property(nonatomic, copy) NSString *lastError;
- (void)startAtBase:(NSString *)base completion:(void (^)(BOOL))completion;
- (void)stopWithCompletion:(void (^)(NSString *))completion;
@end

@implementation SnackRecordingBridge

- (instancetype)init {
    self = [super init];
    if (self) {
        _engine = [[AVAudioEngine alloc] init];
        _audioQueue = dispatch_queue_create("cn.yaowutech.snack.recording-audio", DISPATCH_QUEUE_SERIAL);
    }
    return self;
}

- (void)startAtBase:(NSString *)base completion:(void (^)(BOOL))completion {
    if (@available(macOS 13.0, *)) {
        if (self.stream || self.engine.isRunning) return [self fail:@"已有录音正在进行" completion:completion];
        self.outputBase = base;
        [AVCaptureDevice requestAccessForMediaType:AVMediaTypeAudio completionHandler:^(BOOL granted) {
            if (!granted) return [self fail:@"请在系统设置中允许 Snack 访问麦克风" completion:completion];
            [self beginSystemCapture:completion];
        }];
    } else {
        [self fail:@"会议录音需要 macOS 13 或更高版本" completion:completion];
    }
}

- (void)beginSystemCapture:(void (^)(BOOL))completion API_AVAILABLE(macos(13.0)) {
    [SCShareableContent getShareableContentExcludingDesktopWindows:NO onScreenWindowsOnly:NO completionHandler:^(SCShareableContent *content, NSError *error) {
        if (error || content.displays.count == 0) return [self fail:@"请允许 Snack 访问屏幕与系统音频" completion:completion];
        SCDisplay *display = content.displays.firstObject;
        for (SCDisplay *candidate in content.displays) if (candidate.displayID == CGMainDisplayID()) display = candidate;
        SCContentFilter *filter = [[SCContentFilter alloc] initWithDisplay:display excludingWindows:@[]];
        SCStreamConfiguration *configuration = [[SCStreamConfiguration alloc] init];
        configuration.capturesAudio = YES;
        configuration.excludesCurrentProcessAudio = YES;
        configuration.sampleRate = 48000;
        configuration.channelCount = 2;
        configuration.width = 2;
        configuration.height = 2;
        configuration.minimumFrameInterval = CMTimeMake(1, 1);
        if (![self prepareSystemWriter]) return [self fail:self.lastError completion:completion];
        self.stream = [[SCStream alloc] initWithFilter:filter configuration:configuration delegate:self];
        NSError *outputError = nil;
        if (![self.stream addStreamOutput:self type:SCStreamOutputTypeAudio sampleHandlerQueue:self.audioQueue error:&outputError]) return [self fail:outputError.localizedDescription completion:completion];
        [self.stream startCaptureWithCompletionHandler:^(NSError *startError) {
            if (startError) return [self fail:@"无法开始采集系统音频，请检查录屏权限" completion:completion];
            BOOL started = [self beginMicrophoneCapture];
            if (!started) [self cancelCapture];
            completion(started);
        }];
    }];
}

- (BOOL)prepareSystemWriter {
    self.systemURL = [NSURL fileURLWithPath:[self.outputBase stringByAppendingString:@"-system.m4a"]];
    [NSFileManager.defaultManager removeItemAtURL:self.systemURL error:nil];
    NSError *error = nil;
    self.systemWriter = [[AVAssetWriter alloc] initWithURL:self.systemURL fileType:AVFileTypeAppleM4A error:&error];
    NSDictionary *settings = @{ AVFormatIDKey: @(kAudioFormatMPEG4AAC), AVSampleRateKey: @48000, AVNumberOfChannelsKey: @2, AVEncoderBitRateKey: @128000 };
    self.systemInput = [AVAssetWriterInput assetWriterInputWithMediaType:AVMediaTypeAudio outputSettings:settings];
    self.systemInput.expectsMediaDataInRealTime = YES;
    if (error || ![self.systemWriter canAddInput:self.systemInput]) { self.lastError = @"无法准备系统音频文件"; return NO; }
    [self.systemWriter addInput:self.systemInput];
    self.writerStarted = NO;
    return YES;
}

- (BOOL)beginMicrophoneCapture {
    self.microphoneURL = [NSURL fileURLWithPath:[self.outputBase stringByAppendingString:@"-microphone.wav"]];
    [NSFileManager.defaultManager removeItemAtURL:self.microphoneURL error:nil];
    AVAudioInputNode *input = self.engine.inputNode;
    AVAudioFormat *format = [input outputFormatForBus:0];
    NSError *error = nil;
    self.microphoneFile = [[AVAudioFile alloc] initForWriting:self.microphoneURL settings:format.settings commonFormat:format.commonFormat interleaved:format.isInterleaved error:&error];
    if (error || !self.microphoneFile) { self.lastError = @"无法准备麦克风录音"; return NO; }
    __weak typeof(self) weakSelf = self;
    [input installTapOnBus:0 bufferSize:2048 format:format block:^(AVAudioPCMBuffer *buffer, AVAudioTime *__unused when) {
        NSError *writeError = nil;
        [weakSelf.microphoneFile writeFromBuffer:buffer error:&writeError];
        if (writeError) weakSelf.lastError = writeError.localizedDescription;
    }];
    self.microphoneTapInstalled = YES;
    [self.engine prepare];
    if (![self.engine startAndReturnError:&error]) { self.lastError = error.localizedDescription; return NO; }
    return YES;
}

- (void)stream:(SCStream *)stream didOutputSampleBuffer:(CMSampleBufferRef)sampleBuffer ofType:(SCStreamOutputType)type API_AVAILABLE(macos(13.0)) {
    if (stream != self.stream || type != SCStreamOutputTypeAudio || !CMSampleBufferDataIsReady(sampleBuffer)) return;
    if (!self.writerStarted) {
        if (![self.systemWriter startWriting]) return;
        [self.systemWriter startSessionAtSourceTime:CMSampleBufferGetPresentationTimeStamp(sampleBuffer)];
        self.writerStarted = YES;
    }
    if (self.systemInput.readyForMoreMediaData) [self.systemInput appendSampleBuffer:sampleBuffer];
}

- (void)stream:(SCStream *)stream didStopWithError:(NSError *)error API_AVAILABLE(macos(13.0)) {
    if (stream == self.stream && error) self.lastError = error.localizedDescription;
}

- (void)stopWithCompletion:(void (^)(NSString *))completion {
    if (!self.stream && !self.engine.isRunning) { self.lastError = @"当前没有正在进行的录音"; return completion(nil); }
    if (self.microphoneTapInstalled) [self.engine.inputNode removeTapOnBus:0];
    self.microphoneTapInstalled = NO;
    [self.engine stop];
    self.microphoneFile = nil;
    SCStream *stream = self.stream;
    self.stream = nil;
    [stream stopCaptureWithCompletionHandler:^(NSError *__unused error) {
        dispatch_async(self.audioQueue, ^{ [self finishWriter:completion]; });
    }];
}

- (void)finishWriter:(void (^)(NSString *))completion {
    if (!self.writerStarted || self.systemWriter.status != AVAssetWriterStatusWriting) {
        [self.systemWriter cancelWriting];
        return [self finishWithMicrophone:completion];
    }
    [self.systemInput markAsFinished];
    [self.systemWriter finishWritingWithCompletionHandler:^{ [self exportMixedAudio:completion]; }];
}

- (void)exportMixedAudio:(void (^)(NSString *))completion {
    AVMutableComposition *composition = [AVMutableComposition composition];
    BOOL added = [self addAudioAtURL:self.microphoneURL toComposition:composition];
    added = [self addAudioAtURL:self.systemURL toComposition:composition] || added;
    if (!added) return [self finishWithMicrophone:completion];
    NSString *output = [self.outputBase stringByAppendingString:@".m4a"];
    NSURL *outputURL = [NSURL fileURLWithPath:output];
    [NSFileManager.defaultManager removeItemAtURL:outputURL error:nil];
    AVAssetExportSession *exporter = [[AVAssetExportSession alloc] initWithAsset:composition presetName:AVAssetExportPresetAppleM4A];
    exporter.outputURL = outputURL;
    exporter.outputFileType = AVFileTypeAppleM4A;
    [exporter exportAsynchronouslyWithCompletionHandler:^{
        if (exporter.status == AVAssetExportSessionStatusCompleted) [self cleanupAndComplete:output completion:completion];
        else { self.lastError = exporter.error.localizedDescription ?: @"会议音频合成失败"; [self finishWithMicrophone:completion]; }
    }];
}

- (BOOL)addAudioAtURL:(NSURL *)url toComposition:(AVMutableComposition *)composition {
    AVURLAsset *asset = [AVURLAsset URLAssetWithURL:url options:nil];
    AVAssetTrack *track = [asset tracksWithMediaType:AVMediaTypeAudio].firstObject;
    if (!track) return NO;
    AVMutableCompositionTrack *target = [composition addMutableTrackWithMediaType:AVMediaTypeAudio preferredTrackID:kCMPersistentTrackID_Invalid];
    return [target insertTimeRange:CMTimeRangeMake(kCMTimeZero, asset.duration) ofTrack:track atTime:kCMTimeZero error:nil];
}

- (void)finishWithMicrophone:(void (^)(NSString *))completion {
    NSString *output = [self.outputBase stringByAppendingString:@".wav"];
    [NSFileManager.defaultManager removeItemAtPath:output error:nil];
    NSError *error = nil;
    [NSFileManager.defaultManager moveItemAtPath:self.microphoneURL.path toPath:output error:&error];
    if (error) { self.lastError = error.localizedDescription; completion(nil); }
    else [self cleanupAndComplete:output completion:completion];
}

- (void)cleanupAndComplete:(NSString *)output completion:(void (^)(NSString *))completion {
    [NSFileManager.defaultManager removeItemAtURL:self.microphoneURL error:nil];
    [NSFileManager.defaultManager removeItemAtURL:self.systemURL error:nil];
    self.systemWriter = nil; self.systemInput = nil; self.systemURL = nil; self.microphoneURL = nil; self.outputBase = nil; self.writerStarted = NO;
    completion(output);
}

- (void)fail:(NSString *)message completion:(void (^)(BOOL))completion {
    self.lastError = message.length ? message : @"会议录音启动失败";
    [self cancelCapture];
    completion(NO);
}

- (void)cancelCapture {
    if (self.microphoneTapInstalled) [self.engine.inputNode removeTapOnBus:0];
    self.microphoneTapInstalled = NO;
    [self.engine stop];
    SCStream *stream = self.stream;
    self.stream = nil;
    [stream stopCaptureWithCompletionHandler:nil];
    [self.systemWriter cancelWriting];
    [NSFileManager.defaultManager removeItemAtURL:self.microphoneURL error:nil];
    [NSFileManager.defaultManager removeItemAtURL:self.systemURL error:nil];
    self.systemWriter = nil; self.systemInput = nil; self.microphoneFile = nil; self.microphoneURL = nil; self.systemURL = nil; self.writerStarted = NO;
}
@end

static SnackRecordingBridge *SnackBridge(void) {
    static SnackRecordingBridge *bridge;
    static dispatch_once_t onceToken;
    dispatch_once(&onceToken, ^{ bridge = [[SnackRecordingBridge alloc] init]; });
    return bridge;
}

bool snack_recording_start(const char *base_path) {
    __block BOOL success = NO;
    dispatch_semaphore_t semaphore = dispatch_semaphore_create(0);
    [SnackBridge() startAtBase:[NSString stringWithUTF8String:base_path] completion:^(BOOL started) { success = started; dispatch_semaphore_signal(semaphore); }];
    dispatch_semaphore_wait(semaphore, DISPATCH_TIME_FOREVER);
    return success;
}

char *snack_recording_stop(void) {
    __block NSString *path = nil;
    dispatch_semaphore_t semaphore = dispatch_semaphore_create(0);
    [SnackBridge() stopWithCompletion:^(NSString *result) { path = result; dispatch_semaphore_signal(semaphore); }];
    dispatch_semaphore_wait(semaphore, DISPATCH_TIME_FOREVER);
    return path ? strdup(path.UTF8String) : NULL;
}

char *snack_recording_last_error(void) {
    NSString *message = SnackBridge().lastError ?: @"会议录音失败";
    return strdup(message.UTF8String);
}

void snack_recording_free(char *value) { free(value); }
