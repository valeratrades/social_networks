# My own connections.
{
  path = "/home/v/s/g/rolodex/people";
  tags = {
    ServiceArb = { type = "bool"; };
  };
  procure = {
    servicing = {
      venue = "skool:20kmodropservicingblueprint";
      where = "posts >= 2";
      tags = { ServiceArb = true; };
    };
  };
  rank = [
    { of = "venue_activity"; decay = 3; weight = 1; }
  ];
}
