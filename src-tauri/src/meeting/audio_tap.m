// Core Audio owns the system-audio permission prompt. No ScreenCaptureKit or
// private TCC API is used. This bridge is linked into the app, not a helper.
#import <Foundation/Foundation.h>
#import <CoreAudio/CoreAudio.h>
#import <CoreAudio/AudioHardwareTapping.h>
#import <CoreAudio/CATapDescription.h>

typedef void (*SnackAudioCallback)(const float *, size_t, double, void *);
typedef struct {
    AudioObjectID tap;
    AudioObjectID device;
    AudioDeviceIOProcID proc;
    bool started;
    double sampleRate;
    SnackAudioCallback callback;
    void *context;
} SnackAudioTap;

// The mono tap delivers packed Float32 samples. The callback borrows the buffer
// only for this invocation; Rust copies/resamples into its bounded audio queue.
static OSStatus receiveAudio(AudioObjectID device, const AudioTimeStamp *now,
                             const AudioBufferList *input, const AudioTimeStamp *inputTime,
                             AudioBufferList *output, const AudioTimeStamp *outputTime,
                             void *context) {
    SnackAudioTap *tap = context;
    if (input && input->mNumberBuffers == 1) {
        const AudioBuffer *buffer = &input->mBuffers[0];
        if (buffer->mData && buffer->mNumberChannels == 1 && buffer->mDataByteSize > 0) {
            tap->callback(buffer->mData, buffer->mDataByteSize / sizeof(float),
                          tap->sampleRate, tap->context);
        }
    }
    return noErr;
}

void snack_audio_tap_stop(void *handle) {
    if (!handle) return;
    SnackAudioTap *tap = handle;
    if (tap->started) AudioDeviceStop(tap->device, tap->proc);
    if (tap->proc) AudioDeviceDestroyIOProcID(tap->device, tap->proc);
    if (tap->device) AudioHardwareDestroyAggregateDevice(tap->device);
    if (@available(macOS 14.2, *)) {
        if (tap->tap) AudioHardwareDestroyProcessTap(tap->tap);
    }
    free(tap);
}

// stage: 0 = OS unsupported, 1 = create tap, 2 = format, 3 = aggregate,
// 4 = callback, 5 = start. Return the actual OSStatus rather than describing
// every hardware error as a privacy denial.
void *snack_audio_tap_start(SnackAudioCallback callback, void *context,
                           int32_t *error, uint32_t *stage) {
    *stage = 0;
    *error = 0;
    if (@available(macOS 14.2, *)) {
        @autoreleasepool {
            SnackAudioTap *tap = calloc(1, sizeof(SnackAudioTap));
            if (!tap) { *error = -108; return NULL; }
            tap->callback = callback;
            tap->context = context;
            CATapDescription *description = [[CATapDescription alloc]
                initMonoGlobalTapButExcludeProcesses:@[]];
            description.name = @"Snack meeting system audio";
            description.privateTap = YES;
            description.muteBehavior = CATapUnmuted;
            *stage = 1;
            *error = AudioHardwareCreateProcessTap(description, &tap->tap);
            if (*error) { snack_audio_tap_stop(tap); return NULL; }

            *stage = 2;
            AudioStreamBasicDescription format = {0};
            UInt32 size = sizeof(format);
            AudioObjectPropertyAddress address = { kAudioTapPropertyFormat,
                kAudioObjectPropertyScopeGlobal, kAudioObjectPropertyElementMain };
            *error = AudioObjectGetPropertyData(tap->tap, &address, 0, NULL, &size, &format);
            if (!*error && (format.mFormatID != kAudioFormatLinearPCM ||
                !(format.mFormatFlags & kAudioFormatFlagIsFloat) ||
                (format.mFormatFlags & kAudioFormatFlagIsBigEndian) ||
                format.mBitsPerChannel != 32 || format.mChannelsPerFrame != 1 ||
                format.mBytesPerFrame != sizeof(float) || format.mSampleRate <= 0)) {
                *error = kAudioDeviceUnsupportedFormatError;
            }
            if (*error) { snack_audio_tap_stop(tap); return NULL; }
            tap->sampleRate = format.mSampleRate;
            NSDictionary *aggregate = @{
                @kAudioAggregateDeviceNameKey: @"Snack meeting audio tap",
                @kAudioAggregateDeviceUIDKey: NSUUID.UUID.UUIDString,
                @kAudioAggregateDeviceIsPrivateKey: @YES,
                @kAudioAggregateDeviceTapAutoStartKey: @YES,
                @kAudioAggregateDeviceTapListKey: @[@{
                    @kAudioSubTapUIDKey: description.UUID.UUIDString,
                    @kAudioSubTapDriftCompensationKey: @YES
                }]
            };
            *stage = 3;
            *error = AudioHardwareCreateAggregateDevice((__bridge CFDictionaryRef)aggregate, &tap->device);
            if (*error) { snack_audio_tap_stop(tap); return NULL; }
            *stage = 4;
            *error = AudioDeviceCreateIOProcID(tap->device, receiveAudio, tap, &tap->proc);
            if (*error) { snack_audio_tap_stop(tap); return NULL; }
            *stage = 5;
            // First use triggers macOS's system-audio-only permission dialog.
            *error = AudioDeviceStart(tap->device, tap->proc);
            if (*error) { snack_audio_tap_stop(tap); return NULL; }
            tap->started = true;
            return tap;
        }
    }
    *error = -1;
    return NULL;
}
