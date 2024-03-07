#!/bin/bash

# give 50 or 60 as first command line arg
rate=$1

#helper
copy_file() {
    orig=$(printf "%05d" $1)
    dupl=$(printf "%05d" $2)
    cp ./orig_frames/$orig ./video_frames/$dupl
}

# number of frames per content rate
FRAMES=60
TOTAL_FRAMES=$(( 3 * $FRAMES ))

# copied file index
e=0

do_24_in_60() {
    for (( i = 0; i < $FRAMES; ++i )) ; do
        if (( $i % 2 == 0)); then
            copy_file $i $e
            (( ++e ))
            copy_file $i $e
            (( ++e ))
        else
            copy_file $i $e
            (( ++e ))
            copy_file $i $e
            (( ++e ))
            copy_file $i $e
            (( ++e ))
        fi
    done
}

do_30_in_60() {
    for (( i = $FRAMES; i < 2 * $FRAMES; ++i )) ; do
        copy_file $i $e
        (( ++e ))
        copy_file $i $e
        (( ++e ))
    done
}

do_60_in_60() {
    for (( i = 2 * $FRAMES; i < 3 * $FRAMES; ++i )) ; do
        copy_file $i $e
        (( ++e ))
    done
}

do_25_in_50() {
    for (( i = 0; i < $FRAMES; ++i )) ; do
        copy_file $i $e
        (( ++e ))
        copy_file $i $e
        (( ++e ))
    done
}

do_30_in_50() {
    for (( i = $FRAMES; i < 2 * $FRAMES; ++i )) ; do
        if (( $i % 3 == 0)); then
            copy_file $i $e
            (( ++e ))
            copy_file $i $e
            (( ++e ))
        elif (( $i % 3 == 1)); then
            copy_file $i $e
            (( ++e ))
            copy_file $i $e
            (( ++e ))
        else
            copy_file $i $e
            (( ++e ))
        fi
    done
}

do_50_in_50() {
    for (( i = 2 * $FRAMES; i < 3 * $FRAMES; ++i )) ; do
        copy_file $i $e
        (( ++e ))
    done
}

# prepare
mkdir ./orig_frames
mkdir ./video_frames

# generate test frames
gst-launch-1.0 -v videotestsrc pattern=ball motion=sweep num-buffers=$TOTAL_FRAMES ! \
               video/x-raw,width=800,height=480,format=NV12,framerate=$rate/1 ! \
               multifilesink location="orig_frames/%05d"

# generate video frames with duplicates
video_file=""
case $rate in
    50)
        video_file="25-30-50_in_50.mkv"
        do_25_in_50
        do_30_in_50
        do_50_in_50
        ;;
    60)
        video_file="24-30-60_in_60.mkv"
        do_24_in_60
        do_30_in_60
        do_60_in_60
        ;;
esac

# generate test video
gst-launch-1.0 -v multifilesrc do-timestamp=true location="video_frames/%05d" \
               caps="video/x-raw,width=(int)800,height=(int)480,format=(string)NV12,framerate=(fraction)$rate/1,interlace-mode=(string)progressive" ! \
               vaapih265enc quality-level=7 ! h265parse config-interval=-1 \
               matroskamux ! filesink location=$video_file

# cleanup
rm --force --recursive ./orig_frames ./video_frames
