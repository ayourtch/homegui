# homegui
A simple web GUI + automation to work with zigbee2mqtt

This is a really quick prototype to replace the vendor specific
GUI in my home.

It assumes a certain naming convention of the devices,
the friendly name should look like this:

```
RoomName Device Name - 0xaabbccddeeff
```

This is the naming convention I used in my home,
and this is what I have been testing on.

The short press toggles the status for the lights,
and the long press brings up a modal, which allows
to set the brightness.


