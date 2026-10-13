# Questions for the vendor

WFI-028T (380-415V/3N) full-inverter pool heat pump. We replaced the wired
controller with our own Modbus RTU master, following your protocol document
"Modbus protocol, Zhike inverter pool heat pump, 2022-03-29" (RCDF211268).
It polls the status block (0x0000-0x003e) and the settings block
(0x003f-0x0081) once per second and writes the whole settings block with
function 0x10, three times, like the stock controller. A change of the
manual frequency value alone (see below) is written once.

We now also use the manual frequency (0x003f bit 1 + 0x0043) to follow the
available solar power: the compressor frequency is changed a few times an
hour (at most once per 3 minutes) during sunny hours. Tested on the real
unit: the board follows within about 5 s.

The questions are in order of importance.

---

## 1. Settings storage and write endurance (most important)

1. Where does the mainboard store the settings block 0x003f-0x0081: in
   EEPROM, in the microcontroller's flash, or in RAM with a copy saved
   later? Which part (number) is it?
2. Is the stored copy written on every Modbus write (0x10 or 0x06), even
   when the values are identical? Or only the registers that changed? Or
   only after a delay?
3. What is the rated write endurance of that memory (write cycles), and
   does the firmware do any wear levelling?
4. Are 0x0043 (manual frequency) and 0x003f bit 1 (manual frequency on)
   stored permanently like the other settings? (We see them survive a
   power cut.) Is there a way to set the frequency WITHOUT a permanent
   write - a volatile register, or a register outside the stored block?
5. What write rate do you consider safe for continuous use: writes per
   hour, or total writes over the life of the board?
6. The stock controller writes every change three times, 500 ms apart. Is
   that needed for reliability, or would one write (acknowledged, then
   checked by reading back) be enough?
7. Function 0x06 (write single register): is it supported for 0x0043, and
   does it cause a smaller or no permanent write compared with 0x10?

## 2. Manual frequency mode (0x003f bit 1, 0x0043)

8. What is the manual frequency mode intended for (factory test, service,
   commissioning)? Is continuous use for months acceptable?
9. Units and valid range of 0x0043: Hz, 30-90? What happens outside that
   range?
10. Which protections stay active in manual mode: discharge temperature
    (TP0-TP4), compressor current limit, high/low pressure switches, water
    flow switch, anti-freeze, inverter heatsink temperature? Does the board
    still reduce the frequency by itself when one of them is reached?
11. Is the water setpoint (P01) still respected in manual mode, i.e. does
    the compressor stop when the inlet water reaches the target?
12. Does defrost still work in manual mode: does the board detect icing and
    run a defrost cycle by itself?
13. Is the ECO/silent bit (0x003f bit 6) ignored in manual mode?

## 3. Compressor and oil return

14. Which compressor is fitted (make and model)? A datasheet would answer
    most of the next two questions.
15. What is the minimum frequency for continuous running? Your frequency
    ladder F1-F10 starts at 40 Hz although the allowed range is 30-90 Hz;
    is running for hours at 30-35 Hz acceptable?
16. Does the board run oil-return cycles by itself (periodic higher
    frequency after running long at low frequency), and does it still do so
    in manual mode? If we must do it ourselves: what frequency, for how
    long, and how often?

## 4. Registers the document does not describe

17. 0x001f: the panel shows it as A10 "radiator temperature" (about 40 C).
    Is this the inverter module heatsink? At what temperature does the
    inverter derate or trip?
18. 0x0020 behaves as the compressor current (A09). Which current is it
    (phase current, input current) and what is the scaling?
19. 0x003f bit 12 is set on our unit (0x003f = 0x1071) but not in the
    document. What does it mean?
20. 0x0002 bit 1 sets at every thermostat stop and clears at the next
    start. What is it?
21. 0x0082 ("control switch", read/write): what is it for, and should a
    controller ever write it?

## 5. Differences between the document and the unit

22. Boost/full power: the document puts it in 0x0040 bit 4, but on our
    unit it is 0x003f bit 6, inverted (1 = ECO/silent, 0 = full power), and
    0x0040 bit 4 never changes. Please confirm.
23. 0x003f bit 2 "expansion valve mode: 0 = manual, 1 = automatic": the bit
    is 0 on our unit, yet the valve moves by itself (123-152 steps). Is the
    polarity in the document reversed?
24. Temperature scaling is not in the document. We found: inlet water
    raw / 10, discharge (A04) raw x 1, most others raw / 2. Is that right
    for all temperature registers?

## 6. Low water temperature

25. In spring the compressor does not start until the pool water reaches
    about 10 C; the unit stays on standby until then. Is this the E04
    anti-freeze protection ("ambient/inlet water temp. is too low")? At
    which inlet water and ambient temperatures does it block the start, and
    at which does it release? Is that threshold a parameter (user, installer
    or factory) that can be changed, from the panel or over Modbus? If not,
    is there another way to heat cold water at the start of the season?
