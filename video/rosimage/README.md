# rosimage

This package contains a plugin that subscribes to a ROS1 image topic and acts as a gstreamer video source (`rosimagesrc`), and one for consuming a gstreamer stream and publishing it as a ROS topic (`rosimagesink`).

Related work:
- [ros-gst-bridge](https://github.com/BrettRD/ros-gst-bridge/): the same thing, but for ROS2 (and with more features)
- [gscam](https://github.com/ros-drivers/gscam): ROS1, but only the gstreamer → ROS part.

## Installation

If you have not already, install ROS on your system, according to the [official guide](http://wiki.ros.org/noetic/Installation). The only ROS dependency this plugin has is the `sensor_msgs` package, which is included in `ros-noetic-desktop`. After installing, add ROS to your environment (`source /opt/ros/noetic/setup.bash`) and build the plugin by running `cargo cbuild -p gst-plugin-rosimage` from the root of this repo.

## Hello world

Open four terminals, and run `source /opt/ros/one/setup.bash` in each. Then:
- In the first one, start `roscore`
- In the second one, start a gstreamer → ROS pipeline: `GST_PLUGIN_PATH="target/x86_64-unknown-linux-gnu/debug:$GST_PLUGIN_PATH" gst-launch-1.0 videotestsrc ! rosimagesink topic=/helloworld`
- In the third one, start `rqt_image_view` and select the `/helloworld` topic from the dropdown menu. You should now be seeing the gstreamer test video, but from the ROS side of things.
- In the fourth terminal, start the ROS → gstreamer pipeline `GST_PLUGIN_PATH="target/x86_64-unknown-linux-gnu/debug:$GST_PLUGIN_PATH" gst-launch-1.0 rosimagesrc topic=/helloworld ! videoconvert ! autovideosink`. This will produce a GStreamer window showing the same test video, but after it has been converted back from ROS.

## Copyright

© 2023 by [farming revolution GmbH](https://farming-revolution.com/), published under the [MIT License](../../LICENSE-MIT) or the [Mozilla Public License Version 2.0](../../LICENSE-MPL-2.0), at your choice.